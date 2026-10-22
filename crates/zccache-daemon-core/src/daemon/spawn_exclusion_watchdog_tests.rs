use super::*;
use std::time::Duration;

fn assert_terminates(case: &str, budget: Duration, kind: &str) {
    let output = zccache_core::wait::output_for(
        kernal_api::SpawnSpec::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon::spawn_exclusion::watchdog_tests::watchdog_child",
            ])
            .env("ZCCACHE_SPAWN_WATCHDOG_TEST_CASE", case),
        budget + Duration::from_secs(5),
    )
    .expect("spawn/materialization holder must terminate within its hold budget");
    assert_ne!(output.exit.raw_code(), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("lock hold exceeded"), "{stderr}");
    let holding = stderr
        .lines()
        .find(|line| line.contains(&format!("Holding {kind}")))
        .unwrap_or_else(|| panic!("missing holder: {stderr}"));
    assert!(
        holding.contains("spawn_exclusion_watchdog_tests.rs"),
        "{holding}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("Waiting")
                && line.contains("spawn_exclusion_watchdog_tests.rs")),
        "{stderr}"
    );
}

#[test]
fn watchdog_terminates_wedged_spawn_holder() {
    assert_terminates("spawn", zccache_core::sync::STATE_TIMEOUT, "RwLock(read)");
}

#[test]
fn watchdog_terminates_wedged_materialization_holder() {
    // Exercise the actual production copy budget, without a test-only override.
    assert_terminates(
        "materialize",
        zccache_core::sync::STATE_TIMEOUT * 8,
        "RwLock(write)",
    );
}

fn park_forever() -> ! {
    loop {
        std::thread::park();
    }
}

#[test]
fn watchdog_child() {
    let Ok(case) = std::env::var("ZCCACHE_SPAWN_WATCHDOG_TEST_CASE") else {
        return;
    };
    match case.as_str() {
        "spawn" => {
            let _holder = spawn_shared();
            std::thread::spawn(|| {
                let _waiter = SPAWN_MATERIALIZE_LOCK
                    .write_for(zccache_core::sync::OPERATION_TIMEOUT)
                    .unwrap();
                park_forever();
            });
            park_forever();
        }
        "materialize" => {
            let _holder = materialize_exclusive();
            std::thread::spawn(|| {
                let _waiter = SPAWN_MATERIALIZE_LOCK
                    .read_for(zccache_core::sync::OPERATION_TIMEOUT)
                    .unwrap();
                park_forever();
            });
            park_forever();
        }
        _ => panic!("unknown spawn watchdog case"),
    }
}
