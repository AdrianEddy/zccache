//! Bounded async state access and explicit operation gates.
//!
//! State guards are private. The synchronous callback cannot await or return
//! a borrow of the protected data. Operations that must span awaits use a
//! separate gate carrying no data, with an explicit acquisition budget.

pub use super::sync_barrier::{Barrier, ReadPermit, WritePermit};

use std::sync::Arc;
use std::time::Duration;

/// Budget for a brief in-memory state access.
pub const STATE_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for a queue behind an in-flight compile or maintenance operation.
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A synchronization operation could not acquire its resource in time.
#[derive(Debug, Clone, Copy)]
pub struct LockTimeout(pub Duration);

impl std::fmt::Display for LockTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "synchronization acquisition exceeded {:?}", self.0)
    }
}
impl std::error::Error for LockTimeout {}

/// Data shared by async tasks; no API exposes the mutex or its guard.
#[derive(Debug)]
pub struct State<T> {
    inner: tokio::sync::Mutex<T>,
}

impl<T: Default> Default for State<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> State<T> {
    /// Create protected state.
    pub const fn new(value: T) -> Self {
        Self {
            inner: tokio::sync::Mutex::const_new(value),
        }
    }

    /// Access state synchronously after a bounded async acquisition.
    ///
    /// The callback's return type is independent of its argument's lifetime.
    /// Returning a future borrowing the state is therefore also rejected.
    ///
    /// ```compile_fail
    /// # async fn invalid() {
    /// use zccache_core::sync::{State, STATE_TIMEOUT};
    /// let state = State::new(1);
    /// let borrowed = state.with_for(STATE_TIMEOUT, |value| value).await;
    /// println!("{borrowed}");
    /// # }
    /// ```
    ///
    /// ```compile_fail
    /// # async fn invalid() {
    /// use zccache_core::sync::{State, STATE_TIMEOUT};
    /// let state = State::new(1);
    /// state.with_for(STATE_TIMEOUT, |value| async move {
    ///     std::future::ready(()).await;
    ///     *value += 1;
    /// }).await.await;
    /// # }
    /// ```
    pub async fn try_with_for<R>(
        &self,
        bound: Duration,
        operation: impl FnOnce(&mut T) -> R,
    ) -> Result<R, LockTimeout> {
        let mut guard = tokio::time::timeout(bound, self.inner.lock())
            .await
            .map_err(|_| LockTimeout(bound))?;
        Ok(operation(&mut guard))
    }

    /// Access essential state; fail loudly rather than silently wedge on expiry.
    #[track_caller]
    pub fn with_for<R, F: FnOnce(&mut T) -> R>(
        &self,
        bound: Duration,
        operation: F,
    ) -> impl std::future::Future<Output = R> + use<'_, R, T, F> {
        let site = std::panic::Location::caller();
        async move {
            match self.try_with_for(bound, operation).await {
                Ok(result) => result,
                Err(error) => synchronization_failed(site, error),
            }
        }
    }

    /// Access from a blocking worker. Never call this on an async executor.
    #[track_caller]
    pub fn with_blocking_for<R>(&self, bound: Duration, operation: impl FnOnce(&mut T) -> R) -> R {
        let started = std::time::Instant::now();
        loop {
            if let Ok(mut guard) = self.inner.try_lock() {
                return operation(&mut guard);
            }
            if started.elapsed() >= bound {
                synchronization_failed(std::panic::Location::caller(), LockTimeout(bound));
            }
            std::thread::sleep(
                Duration::from_millis(1).min(bound.saturating_sub(started.elapsed())),
            );
        }
    }
}

/// Serialize whole operations without exposing a protected-data guard.
#[derive(Debug)]
pub struct Gate {
    inner: Arc<tokio::sync::Semaphore>,
}

/// An operation permit retains the gate's identity in weak per-key pools.
/// Release capacity before dropping the last gate owner.
#[derive(Debug)]
pub struct GatePermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
    _gate: Arc<Gate>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    /// Create a gate admitting one operation at a time.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// Acquire one operation permit within a mandatory budget.
    pub async fn try_acquire_for(
        self: &Arc<Self>,
        bound: Duration,
    ) -> Result<GatePermit, LockTimeout> {
        match tokio::time::timeout(bound, Arc::clone(&self.inner).acquire_owned()).await {
            Ok(Ok(permit)) => Ok(GatePermit {
                _permit: permit,
                _gate: Arc::clone(self),
            }),
            _ => Err(LockTimeout(bound)),
        }
    }

    /// Acquire an essential operation permit, diagnosing expiry at the caller.
    #[track_caller]
    pub fn acquire_for(
        self: &Arc<Self>,
        bound: Duration,
    ) -> impl std::future::Future<Output = GatePermit> + use<'_> {
        let site = std::panic::Location::caller();
        async move {
            match self.try_acquire_for(bound).await {
                Ok(permit) => permit,
                Err(error) => synchronization_failed(site, error),
            }
        }
    }

    /// Acquire on a blocking worker, preserving FIFO admission and ownership.
    #[track_caller]
    pub fn blocking_acquire_for(self: &Arc<Self>, bound: Duration) -> GatePermit {
        match super::sync_barrier::blocking_for(Arc::clone(&self.inner).acquire_owned(), bound) {
            Ok(permit) => GatePermit {
                _permit: permit,
                _gate: Arc::clone(self),
            },
            Err(_) => synchronization_failed(std::panic::Location::caller(), LockTimeout(bound)),
        }
    }
}

pub(crate) fn synchronization_failed(site: &std::panic::Location<'_>, error: LockTimeout) -> ! {
    tracing::error!(%site, %error, "bounded synchronization failed");
    eprintln!("bounded synchronization failed at {site}: {error}");
    // Essential state is unavailable. End the service so its supervisor can
    // restart it; a detached task panic can otherwise leave a partial service.
    std::process::abort()
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
