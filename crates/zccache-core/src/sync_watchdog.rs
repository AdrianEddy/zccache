//! Private bookkeeping for short critical sections, independent of async runtimes.
use std::io::Write;
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Waiting,
    Holding,
}

#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    owner: usize,
    kind: &'static str,
    phase: Phase,
    site: &'static Location<'static>,
    thread: String,
    since: Instant,
    limit: Duration,
    reported: bool,
}

fn registry() -> &'static Mutex<Vec<Entry>> {
    static REGISTRY: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    &REGISTRY
}

fn with_entries<R>(operation: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    // The only raw bookkeeping mutex. No user code, I/O or waits run under it.
    operation(&mut registry().lock().unwrap_or_else(PoisonError::into_inner))
}

fn thread_name() -> String {
    let thread = std::thread::current();
    thread
        .name()
        .map_or_else(|| format!("{:?}", thread.id()), str::to_owned)
}

pub(crate) fn dump_context() {
    let entries = with_entries(|entries| entries.clone());
    let _ = std::io::stderr().write_all(report(&entries, Instant::now(), false).as_bytes());
}

#[cfg(test)]
pub(crate) fn registered<T: ?Sized>(owner: &T) -> usize {
    let owner = std::ptr::from_ref(owner).cast::<()>() as usize;
    with_entries(|entries| entries.iter().filter(|entry| entry.owner == owner).count())
}

/// Owns one waiting/holding registration; cancellation and unwinding remove it.
#[derive(Debug)]
pub(crate) struct Ticket(u64);

impl Ticket {
    pub(crate) fn waiting<T: ?Sized>(
        owner: &T,
        kind: &'static str,
        site: &'static Location<'static>,
    ) -> Self {
        start();
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let entry = Entry {
            id,
            owner: std::ptr::from_ref(owner).cast::<()>() as usize,
            kind,
            phase: Phase::Waiting,
            site,
            thread: thread_name(),
            since: Instant::now(),
            limit: super::sync::STATE_TIMEOUT,
            reported: false,
        };
        with_entries(|entries| entries.push(entry));
        Self(id)
    }

    pub(crate) fn holding(&mut self, limit: Duration) {
        let thread = thread_name();
        with_entries(|entries| {
            if let Some(entry) = entries.iter_mut().find(|entry| entry.id == self.0) {
                entry.phase = Phase::Holding;
                entry.since = Instant::now();
                entry.limit = limit;
                entry.reported = false;
                entry.thread = thread;
            }
        });
    }
}

#[cfg(test)]
#[path = "sync_watchdog_registry_tests.rs"]
mod tests;

impl Drop for Ticket {
    fn drop(&mut self) {
        with_entries(|entries| {
            if let Some(index) = entries.iter().position(|entry| entry.id == self.0) {
                entries.swap_remove(index);
            }
        });
    }
}

/// Formatting and stderr writes run outside the registry mutex.
fn report(entries: &[Entry], now: Instant, abort: bool) -> String {
    use std::fmt::Write;
    let phase = if abort { "hold" } else { "wait" };
    let mut text = format!("zccache: lock {phase} exceeded its budget. Registered sites:\n");
    for entry in entries {
        let _ = writeln!(
            text,
            "  {:?} {} owner={:#x} age_ns={} budget_ns={} at {} (thread {})",
            entry.phase,
            entry.kind,
            entry.owner,
            now.saturating_duration_since(entry.since).as_nanos(),
            entry.limit.as_nanos(),
            entry.site,
            entry.thread,
        );
    }
    text
}

fn decision(entries: &mut [Entry], now: Instant) -> Option<(bool, Vec<Entry>)> {
    let mut abort = false;
    let mut notify = false;
    for entry in entries.iter_mut() {
        if now.saturating_duration_since(entry.since) < entry.limit {
            continue;
        }
        if entry.phase == Phase::Holding {
            abort = true;
        } else if !entry.reported {
            entry.reported = true;
            notify = true;
        }
    }
    (abort || notify).then(|| (abort, entries.to_vec()))
}

fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        if let Err(error) = std::thread::Builder::new()
            .name("zccache-lock-watchdog".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_millis(100));
                let now = Instant::now();
                let observation = with_entries(|entries| decision(entries, now));
                if let Some((abort, entries)) = observation {
                    // Direct I/O survives libtest's thread-local print capture.
                    let _ = std::io::stderr().write_all(report(&entries, now, abort).as_bytes());
                    if abort {
                        std::process::abort();
                    }
                }
            })
        {
            let _ = writeln!(
                std::io::stderr(),
                "zccache: lock watchdog could not start: {error}"
            );
            // A missing backstop must not silently downgrade the safety contract.
            std::process::abort();
        }
    });
}
