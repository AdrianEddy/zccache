//! Timeout-only synchronous locks. Async callers must use [`super::sync::State`].
//!
//! Poisoning retains std's semantics: callers deciding to fail on poison must
//! not silently recover partially mutated cache metadata. Acquisition expiry
//! fails loudly at the caller, rather than leaving an executor wedged.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{
    LockResult, MutexGuard, RwLockReadGuard, RwLockWriteGuard, TryLockError, TryLockResult,
};
use std::time::{Duration, Instant};

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
#[derive(Debug, Default)]
pub struct Mutex<T: ?Sized> {
    inner: std::sync::Mutex<T>,
}

impl<T> Mutex<T> {
    /// Create a mutex.
    pub const fn new(value: T) -> Self {
        Self {
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
        acquire_for(bound, || self.inner.try_lock())
    }
    /// Attempt without blocking.
    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        self.inner.try_lock()
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
#[derive(Debug, Default)]
pub struct RwLock<T: ?Sized> {
    waiting_writers: AtomicUsize,
    inner: std::sync::RwLock<T>,
}
impl<T> RwLock<T> {
    /// Create an unlocked value.
    pub const fn new(value: T) -> Self {
        Self {
            waiting_writers: AtomicUsize::new(0),
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
        acquire_for(bound, || {
            if self.waiting_writers.load(Ordering::Acquire) > 0 {
                Err(TryLockError::WouldBlock)
            } else {
                self.inner.try_read()
            }
        })
    }
    /// Acquire exclusive access within a mandatory budget.
    #[track_caller]
    pub fn write_for(&self, bound: Duration) -> LockResult<RwLockWriteGuard<'_, T>> {
        // Polling try_write alone never joins std's native writer queue:
        // continuous readers could starve the writer forever. Publish writer
        // intent before polling so later readers preserve writer preference.
        self.waiting_writers.fetch_add(1, Ordering::AcqRel);
        let _waiting = WaitingWriter(&self.waiting_writers);
        acquire_for(bound, || self.inner.try_write())
    }
    /// Attempt shared access without blocking.
    pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
        if self.waiting_writers.load(Ordering::Acquire) > 0 {
            Err(TryLockError::WouldBlock)
        } else {
            self.inner.try_read()
        }
    }
    /// Attempt exclusive access without blocking.
    pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
        self.inner.try_write()
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
