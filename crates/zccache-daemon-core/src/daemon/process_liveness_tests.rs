//! Portable subprocess regression for the generic linker/exec/tool path.
use super::*;
use std::time::{Duration, Instant};

const CASE: &str = "ZCCACHE_TEST_GENERIC_LIVENESS";
const PID_FILE: &str = "ZCCACHE_TEST_GENERIC_LIVENESS_PID";
const FIXTURE: &str = "daemon::process::liveness_tests::generic_tool_liveness_fixture";

fn assert_controller(case: &str) {
    let output = zccache_core::wait::output_for(
        kernal_api::SpawnSpec::new(std::env::current_exe().unwrap())
            .args(["--exact", FIXTURE, "--nocapture"])
            .env(CASE, case)
            .env("ZCCACHE_STALL_WINDOW_MS", "2000")
            .env("ZCCACHE_POST_EXIT_DRAIN_MS", "100"),
        Duration::from_secs(30),
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
fn silent_generic_tool_expires_reaps_and_releases_output_gate() {
    assert_controller("controller");
}
#[test]
fn generic_tool_output_progress_outlives_its_stall_window() {
    assert_controller("output-controller");
}
#[test]
fn generic_tool_cpu_progress_outlives_its_stall_window() {
    assert_controller("cpu-controller");
}
#[test]
fn generic_tool_total_deadline_confirms_reaping() {
    assert_controller("runtime-controller");
}
#[test]
fn generic_tool_cancellation_reaps_and_releases_output_gate() {
    assert_controller("cancel-controller");
}

#[test]
fn process_policy_rejects_disabled_and_unrepresentable_budgets() {
    let second = Duration::from_secs(1);
    for (stall, total, cleanup) in [
        (Duration::ZERO, second, second),
        (second, Duration::ZERO, second),
        (second, second, Duration::ZERO),
        (second, Duration::MAX, second),
    ] {
        assert_eq!(
            ProcessLiveness::new(stall, total, second, cleanup)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert!(
        ProcessLiveness::new(second, second, Duration::ZERO, second).is_ok(),
        "immediate abandonment is bounded; it must not select strict EOF"
    );
}

async fn wait_for_pid(path: &std::path::Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.parse() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "tool never published readiness");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn generic_tool_liveness_fixture() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    if matches!(case.as_str(), "silent-child" | "output-child" | "cpu-child") {
        std::fs::write(
            std::env::var_os(PID_FILE).unwrap(),
            std::process::id().to_string(),
        )
        .unwrap();
        match case.as_str() {
            "silent-child" => loop {
                std::thread::park();
            },
            "output-child" => {
                for _ in 0..50 {
                    println!("output-progress");
                    std::thread::sleep(Duration::from_millis(40));
                }
            }
            "cpu-child" => {
                let until = Instant::now() + Duration::from_secs(2);
                let mut value = 1_u64;
                while Instant::now() < until {
                    value = std::hint::black_box(value.wrapping_mul(17).wrapping_add(3));
                }
                std::hint::black_box(value);
            }
            _ => unreachable!(),
        }
        println!("tool-finished");
        return;
    }
    assert!(matches!(
        case.as_str(),
        "controller"
            | "output-controller"
            | "cpu-controller"
            | "runtime-controller"
            | "cancel-controller"
    ));
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("tool.pid");
    let child_case = match case.as_str() {
        "output-controller" => "output-child",
        "cpu-controller" => "cpu-child",
        _ => "silent-child",
    };
    let builder = kernal_api::SpawnSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", FIXTURE, "--nocapture"])
        .env(CASE, child_case)
        .env(PID_FILE, pid_file.as_os_str());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let gate = Arc::new(zccache_core::sync::Gate::new());
        let task_gate = Arc::clone(&gate);
        let policy = match case.as_str() {
            "runtime-controller" => ProcessLiveness::new(
                Duration::from_secs(30),
                Duration::from_secs(2),
                Duration::from_millis(100),
                Duration::from_secs(2),
            )
            .unwrap(),
            "output-controller" | "cpu-controller" => ProcessLiveness::new(
                Duration::from_secs(1),
                Duration::from_secs(20),
                Duration::from_millis(100),
                Duration::from_secs(2),
            )
            .unwrap(),
            _ => ProcessLiveness::configured(),
        };
        let operation = async move {
            let _output_gate = task_gate.acquire_for(Duration::from_secs(1)).await;
            async_builder_output_with_priority(builder, CompilePriority::Normal, policy).await
        };
        let pid = if case == "cancel-controller" {
            let task = tokio::spawn(operation);
            let pid = wait_for_pid(&pid_file).await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            pid
        } else {
            let result = tokio::time::timeout(Duration::from_secs(15), operation)
                .await
                .expect("generic session bypassed its liveness policy");
            if case == "runtime-controller" {
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
            } else {
                let output = result.expect("watchdog must return the terminated child's result");
                if matches!(case.as_str(), "output-controller" | "cpu-controller") {
                    assert!(output.status.success(), "a progressing tool was killed");
                    assert!(String::from_utf8_lossy(&output.stdout).contains("tool-finished"));
                } else {
                    assert!(!output.status.success(), "silent tool was not terminated");
                }
            }
            wait_for_pid(&pid_file).await
        };
        assert!(
            gate.try_acquire().is_ok(),
            "completed/cancelled tool retained its output gate"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while crate::platform::process::inspect::is_alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !crate::platform::process::inspect::is_alive(pid),
            "owned child was not reaped"
        );
    });
}
