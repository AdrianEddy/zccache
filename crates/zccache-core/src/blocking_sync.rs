//! Timeout-only synchronous locks. Async callers must use [`super::sync::State`].
//!
//! Poisoning retains std's semantics: callers deciding to fail on poison must
//! not silently recover partially mutated cache metadata. Acquisition expiry
//! fails loudly at the caller, rather than leaving an executor wedged.

use super::sync_watchdog::Ticket;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LockResult, PoisonError, TryLockError, TryLockResult};
use std::time::{Duration, Instant};

fn tracked<G, W>(
    result: LockResult<G>,
    mut ticket: Ticket,
    limit: Duration,
    wrap: impl FnOnce(G, Ticket) -> W,
) -> LockResult<W> {
    let (inner, poisoned) = match result {
        Ok(inner) => (inner, false),
        Err(error) => (error.into_inner(), true),
    };
    ticket.holding(limit);
    let guard = wrap(inner, ticket);
    if poisoned {
        Err(PoisonError::new(guard))
    } else {
        Ok(guard)
    }
}

fn tracked_try<G, W>(
    result: TryLockResult<G>,
    ticket: Ticket,
    limit: Duration,
    wrap: impl FnOnce(G, Ticket) -> W,
) -> TryLockResult<W> {
    let result = match result {
        Ok(inner) => Ok(inner),
        Err(TryLockError::Poisoned(error)) => Err(error),
        Err(TryLockError::WouldBlock) => return Err(TryLockError::WouldBlock),
    };
    tracked(result, ticket, limit, wrap).map_err(TryLockError::Poisoned)
}

/// Native mutex ownership, registered until the underlying guard releases.
#[derive(Debug)]
pub struct MutexGuard<'a, T: ?Sized> {
    inner: std::sync::MutexGuard<'a, T>,
    _ticket: Ticket,
}
/// Native shared ownership with holder diagnostics.
#[derive(Debug)]
pub struct RwLockReadGuard<'a, T: ?Sized> {
    inner: std::sync::RwLockReadGuard<'a, T>,
    _ticket: Ticket,
}
/// Native exclusive ownership with holder diagnostics.
#[derive(Debug)]
pub struct RwLockWriteGuard<'a, T: ?Sized> {
    inner: std::sync::RwLockWriteGuard<'a, T>,
    _ticket: Ticket,
}

macro_rules! deref_guard {
    ($guard:ident) => {
        impl<T: ?Sized> std::ops::Deref for $guard<'_, T> {
            type Target = T;
            fn deref(&self) -> &T {
                &self.inner
            }
        }
    };
}
deref_guard!(MutexGuard);
deref_guard!(RwLockReadGuard);
deref_guard!(RwLockWriteGuard);
impl<T: ?Sized> std::ops::DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}
impl<T: ?Sized> std::ops::DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}
impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

#[track_caller]
fn acquire_for<G>(bound: Duration, mut attempt: impl FnMut() -> TryLockResult<G>) -> LockResult<G> {
    let started = Instant::now();
    let mut backoff = Duration::from_micros(50);
    loop {
        match attempt() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => return Err(error),
            Err(TryLockError::WouldBlock) => {}
        }
        if started.elapsed() >= bound {
            super::sync::synchronization_failed(
                std::panic::Location::caller(),
                super::sync::LockTimeout(bound),
            );
        }
        std::thread::sleep(backoff.min(bound.saturating_sub(started.elapsed())));
        backoff = (backoff * 2).min(Duration::from_millis(5));
    }
}

/// A synchronous mutex whose blocking acquisition requires a budget.
#[derive(Debug)]
pub struct Mutex<T: ?Sized> {
    hold_limit: Duration,
    inner: std::sync::Mutex<T>,
}

impl<T> Mutex<T> {
    /// Create a mutex.
    pub const fn new(value: T) -> Self {
        Self::with_hold_limit(value, super::sync::STATE_TIMEOUT)
    }
    /// Create a critical section with an explicit justified hold budget.
    pub const fn with_hold_limit(value: T, hold_limit: Duration) -> Self {
        Self {
            hold_limit,
            inner: std::sync::Mutex::new(value),
        }
    }
    /// Consume the mutex, preserving poisoning.
    pub fn into_inner(self) -> LockResult<T> {
        self.inner.into_inner()
    }
}
impl<T: ?Sized> Mutex<T> {
    /// Acquire within `bound`; expiry diagnoses a broken synchronization invariant.
    #[track_caller]
    pub fn lock_for(&self, bound: Duration) -> LockResult<MutexGuard<'_, T>> {
        let ticket = Ticket::waiting(self, "Mutex", std::panic::Location::caller());
        tracked(
            acquire_for(bound, || self.inner.try_lock()),
            ticket,
            self.hold_limit,
            |inner, ticket| MutexGuard {
                inner,
                _ticket: ticket,
            },
        )
    }
    /// Attempt without blocking.
    #[track_caller]
    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        let ticket = Ticket::waiting(self, "Mutex", std::panic::Location::caller());
        tracked_try(
            self.inner.try_lock(),
            ticket,
            self.hold_limit,
            |inner, ticket| MutexGuard {
                inner,
                _ticket: ticket,
            },
        )
    }
    /// Access exclusively borrowed data without locking.
    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        self.inner.get_mut()
    }
    /// Whether a holder panicked.
    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }
}

/// A synchronous reader/writer lock with mandatory acquisition budgets.
#[derive(Debug)]
pub struct RwLock<T: ?Sized> {
    waiting_writers: AtomicUsize,
    read_hold_limit: Duration,
    write_hold_limit: Duration,
    inner: std::sync::RwLock<T>,
}
impl<T> RwLock<T> {
    /// Create an unlocked value.
    pub const fn new(value: T) -> Self {
        Self::with_hold_limits(
            value,
            super::sync::STATE_TIMEOUT,
            super::sync::STATE_TIMEOUT,
        )
    }
    /// Give shared native spawn and exclusive artifact copies separate hold budgets.
    pub const fn with_hold_limits(
        value: T,
        read_hold_limit: Duration,
        write_hold_limit: Duration,
    ) -> Self {
        Self {
            waiting_writers: AtomicUsize::new(0),
            read_hold_limit,
            write_hold_limit,
            inner: std::sync::RwLock::new(value),
        }
    }
    /// Consume the lock, preserving poisoning.
    pub fn into_inner(self) -> LockResult<T> {
        self.inner.into_inner()
    }
}
impl<T: ?Sized> RwLock<T> {
    /// Acquire shared access within a mandatory budget.
    #[track_caller]
    pub fn read_for(&self, bound: Duration) -> LockResult<RwLockReadGuard<'_, T>> {
        let ticket = Ticket::waiting(self, "RwLock(read)", std::panic::Location::caller());
        tracked(
            acquire_for(bound, || {
                if self.waiting_writers.load(Ordering::Acquire) > 0 {
                    Err(TryLockError::WouldBlock)
                } else {
                    self.inner.try_read()
                }
            }),
            ticket,
            self.read_hold_limit,
            |inner, ticket| RwLockReadGuard {
                inner,
                _ticket: ticket,
            },
        )
    }
    /// Acquire exclusive access within a mandatory budget.
    #[track_caller]
    pub fn write_for(&self, bound: Duration) -> LockResult<RwLockWriteGuard<'_, T>> {
        // Polling try_write alone never joins std's native writer queue:
        // continuous readers could starve the writer forever. Publish writer
        // intent before polling so later readers preserve writer preference.
        self.waiting_writers.fetch_add(1, Ordering::AcqRel);
        let _waiting = WaitingWriter(&self.waiting_writers);
        let ticket = Ticket::waiting(self, "RwLock(write)", std::panic::Location::caller());
        tracked(
            acquire_for(bound, || self.inner.try_write()),
            ticket,
            self.write_hold_limit,
            |inner, ticket| RwLockWriteGuard {
                inner,
                _ticket: ticket,
            },
        )
    }
    /// Attempt shared access without blocking.
    #[track_caller]
    pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
        if self.waiting_writers.load(Ordering::Acquire) > 0 {
            Err(TryLockError::WouldBlock)
        } else {
            let ticket = Ticket::waiting(self, "RwLock(read)", std::panic::Location::caller());
            tracked_try(
                self.inner.try_read(),
                ticket,
                self.read_hold_limit,
                |inner, ticket| RwLockReadGuard {
                    inner,
                    _ticket: ticket,
                },
            )
        }
    }
    /// Attempt exclusive access without blocking.
    #[track_caller]
    pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
        let ticket = Ticket::waiting(self, "RwLock(write)", std::panic::Location::caller());
        tracked_try(
            self.inner.try_write(),
            ticket,
            self.write_hold_limit,
            |inner, ticket| RwLockWriteGuard {
                inner,
                _ticket: ticket,
            },
        )
    }
    /// Access exclusively borrowed data without locking.
    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        self.inner.get_mut()
    }
}

struct WaitingWriter<'a>(&'a AtomicUsize);
impl Drop for WaitingWriter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_writer_prevents_later_readers_from_bypassing_it() {
        let lock = RwLock::new(0);
        std::thread::scope(|scope| {
            let reader = lock.read_for(Duration::from_secs(1)).unwrap();
            let writer = scope.spawn(|| {
                *lock.write_for(Duration::from_secs(5)).unwrap() = 1;
            });
            let started = Instant::now();
            while lock.waiting_writers.load(Ordering::Acquire) == 0
                && started.elapsed() < Duration::from_secs(1)
            {
                std::thread::yield_now();
            }
            assert_eq!(lock.waiting_writers.load(Ordering::Acquire), 1);
            assert!(matches!(lock.try_read(), Err(TryLockError::WouldBlock)));
            drop(reader);
            writer.join().unwrap();
        });
        assert_eq!(*lock.read_for(Duration::from_secs(1)).unwrap(), 1);
    }
}
