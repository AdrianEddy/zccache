//! Get real-core UI link inputs without parsing Cargo's wrapped compiler argv.

pub(super) fn core_flags() -> Vec<String> {
    let toolchain = std::env::var("RUSTUP_TOOLCHAIN").expect("UI toolchain must be explicit");
    let output = std::process::Command::new("soldr")
        // Match ci.yml's DYLINT_CARGO_SHIM: this is a new managed toolchain
        // invocation, not a recursive front-door dispatch. Keep the compiler
        // wrapper and explicit nightly toolchain intact.
        .env_remove("IN_SOLDR_PID")
        .args([
            "rustup",
            "run",
            &toolchain,
            "cargo",
            "build",
            "--examples",
            "--message-format=json",
        ])
        .output()
        .expect("soldr must build the UI dependency examples");
    assert!(
        output.status.success(),
        "UI dependency build failed: {}",
        String::from_utf8_lossy(&output.stderr[output.stderr.len().saturating_sub(4000)..])
    );
    let core = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| {
            if message["reason"] != "compiler-artifact"
                || message["target"]["name"] != "zccache_core"
            {
                return None;
            }
            message["filenames"]
                .as_array()?
                .iter()
                .filter_map(|name| name.as_str())
                .find(|name| name.ends_with(".rlib"))
                .map(std::path::PathBuf::from)
        })
        .expect("Cargo JSON must identify the actual core dependency artifact");
    let dependencies = core
        .parent()
        .expect("core artifact must be in a dependency directory");
    vec![
        "--edition=2021".into(),
        format!("-Ldependency={}", dependencies.display()),
        format!("--extern=zccache_core={}", core.display()),
    ]
}
