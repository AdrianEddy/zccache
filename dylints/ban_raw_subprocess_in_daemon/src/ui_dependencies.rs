//! Get real-core UI link inputs without parsing Cargo's wrapped compiler argv.

pub(super) fn dependency_flags() -> Vec<String> {
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
    let messages: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let mut flags = vec!["--edition=2021".into()];
    for name in ["zccache_core", "kernal_api", "tokio"] {
        let artifact = messages
            .iter()
            .find_map(|message| {
                if message["reason"] != "compiler-artifact" || message["target"]["name"] != name {
                    return None;
                }
                message["filenames"]
                    .as_array()?
                    .iter()
                    .filter_map(|value| value.as_str())
                    .find(|path| path.ends_with(".rlib"))
            })
            .unwrap_or_else(|| panic!("Cargo JSON missing {name} artifact"));
        let artifact = std::path::Path::new(artifact);
        flags.push(format!(
            "-Ldependency={}",
            artifact.parent().unwrap().display()
        ));
        flags.push(format!("--extern={name}={}", artifact.display()));
    }
    flags
}
