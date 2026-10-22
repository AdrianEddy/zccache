//! Portable subprocess regression for the generic linker/exec/tool path.
use super::*;
use std::time::{Duration, Instant};

const CASE: &str = "ZCCACHE_TEST_GENERIC_LIVENESS";
const PID_FILE: &str = "ZCCACHE_TEST_GENERIC_LIVENESS_PID";
const FIXTURE: &str = "daemon::process::liveness_tests::generic_tool_liveness_fixture";

#[test]
fn silent_generic_tool_expires_reaps_and_releases_output_gate() {
    // Configure only this contained controller, never the parent test process.
    let output = zccache_core::wait::output_for(
        kernal_api::SpawnSpec::new(std::env::current_exe().unwrap())
            .args(["--exact", FIXTURE, "--nocapture"])
            .env(CASE, "controller")
            .env("ZCCACHE_STALL_WINDOW_MS", "50")
            .env("ZCCACHE_POST_EXIT_DRAIN_MS", "100"),
        Duration::from_secs(25),
    )
    .expect("generic liveness controller must terminate");
    assert_eq!(
        output.exit.raw_code(),
        0,
        "controller failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generic_tool_liveness_fixture() {
    match std::env::var(CASE).as_deref() {
        Ok("silent-child") => {
            std::fs::write(
                std::env::var_os(PID_FILE).unwrap(),
                std::process::id().to_string(),
            )
            .unwrap();
            loop {
                std::thread::park();
            }
        }
        Ok("controller") => {
            let directory = tempfile::tempdir().unwrap();
            let pid_file = directory.path().join("silent-tool.pid");
            let builder = kernal_api::SpawnSpec::new(std::env::current_exe().unwrap())
                .args(["--exact", FIXTURE, "--nocapture"])
                .env(CASE, "silent-child")
                .env(PID_FILE, pid_file.as_os_str());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let gate = Arc::new(zccache_core::sync::Gate::new());
                let operation = async {
                    let _output_gate = gate.acquire_for(Duration::from_secs(1)).await;
                    async_builder_output_with_priority(builder, CompilePriority::Normal).await
                };
                // The existing compiler sampling cadence is five seconds;
                // allow two samples plus cleanup while requiring autonomous expiry.
                let output = tokio::time::timeout(Duration::from_secs(15), operation)
                    .await
                    .expect("silent live generic child bypassed the stall watchdog")
                    .expect("watchdog must return the terminated child's result");
                assert!(!output.status.success(), "silent tool was not terminated");
                assert!(
                    gate.try_acquire().is_ok(),
                    "expired tool retained its output gate"
                );
                let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while crate::platform::process::inspect::is_alive(pid) && Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(
                    !crate::platform::process::inspect::is_alive(pid),
                    "expired child was not reaped"
                );
            });
        }
        Err(_) => {}
        Ok(_) => panic!("unknown generic liveness fixture case"),
    }
}
