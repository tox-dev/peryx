#[cfg(not(windows))]
use std::path::Path;
use std::process::Command;

#[cfg(not(windows))]
fn fake_cargo(directory: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let path = directory.join("cargo");
    std::fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = metadata ]; then printf '{\"target_directory\":\"%s\"}\\n' \"$PERYX_FAKE_TARGET\"; fi\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn cli_prints_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_peryx-bench"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
}

#[cfg(not(windows))]
#[test]
fn cli_runs_with_every_workload_disabled() {
    let directory = tempfile::tempdir().unwrap();
    let tools = directory.path().join("bin");
    std::fs::create_dir(&tools).unwrap();
    fake_cargo(&tools);
    let path =
        std::env::join_paths(std::iter::once(tools).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
            .unwrap();
    let target = directory.path().join("target");
    let output = Command::new(env!("CARGO_BIN_EXE_peryx-bench"))
        .args([
            "--skip",
            "machine",
            "--skip",
            "install",
            "--skip",
            "throughput",
            "--skip",
            "parallel",
            "--skip",
            "metadata",
            "--skip",
            "load",
            "--skip",
            "endpoints",
            "--only",
            "missing",
            "--report",
        ])
        .arg(directory.path().join("report.toml"))
        .env("PATH", path)
        .env("PERYX_FAKE_TARGET", target.to_string_lossy().replace('\\', "/"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
