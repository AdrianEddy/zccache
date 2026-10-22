//! soldr#3836: a running compiler probe cannot own spawn admission.

use super::{output_within, output_within_async, ProbeOutcome};
use std::time::{Duration, Instant};

fn check_probe_releases_admission(async_probe: bool) {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let release = dir.path().join("release");
    let spec = if crate::platform::host::is_windows() {
        kernal_api::SpawnSpec::new("powershell").args([
            "-NoProfile", "-NonInteractive", "-Command",
            "[IO.File]::WriteAllText($env:ZCCACHE_TEST_PROBE_READY, 'ready'); while (-not [IO.File]::Exists($env:ZCCACHE_TEST_PROBE_RELEASE)) { Start-Sleep -Milliseconds 20 }",
        ])
    } else {
        kernal_api::SpawnSpec::new("sh").args([
            "-c",
            "printf ready > \"$ZCCACHE_TEST_PROBE_READY\"; while [ ! -f \"$ZCCACHE_TEST_PROBE_RELEASE\" ]; do sleep 0.02; done",
        ])
    }
    .env("ZCCACHE_TEST_PROBE_READY", ready.as_os_str())
    .env("ZCCACHE_TEST_PROBE_RELEASE", release.as_os_str());

    let probe = std::thread::spawn(move || {
        if async_probe {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(output_within_async(spec, Duration::from_secs(20)))
        } else {
            output_within(spec, Duration::from_secs(20))
        }
    });
    let started = Instant::now();
    while !ready.exists() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    // Child readiness proves we are testing an in-flight wait, rather than
    // racing the probe's startup. Release on every assertion path.
    if !ready.exists() {
        let _ = std::fs::write(&release, b"release");
        let _ = probe.join();
        panic!("compiler probe never signalled readiness");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let _guard = crate::daemon::spawn_exclusion::materialize_exclusive();
        let _ = tx.send(());
    });
    let acquired_while_probe_waited = rx.recv_timeout(Duration::from_secs(2)).is_ok();
    std::fs::write(&release, b"release").unwrap();
    let outcome = probe.join().unwrap();
    writer.join().unwrap();
    assert!(
        acquired_while_probe_waited,
        "probe held spawn admission while waiting for the child"
    );
    assert!(matches!(outcome, ProbeOutcome::Completed(output) if output.success));
}

#[test]
fn sync_probe_does_not_hold_spawn_admission_while_waiting() {
    check_probe_releases_admission(false);
}

#[test]
fn async_probe_does_not_hold_spawn_admission_while_waiting() {
    check_probe_releases_admission(true);
}
