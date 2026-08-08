use std::path::Path;
use std::process::Command;

fn fake_cargo(directory: &Path) -> std::path::PathBuf {
    #[cfg(windows)]
    let path = directory.join("cargo.cmd");
    #[cfg(not(windows))]
    let path = directory.join("cargo");

    #[cfg(windows)]
    std::fs::write(
        &path,
        "@echo off\r\nif \"%1\"==\"metadata\" echo {\"target_directory\":\"%PERYX_FAKE_TARGET%\"}\r\nexit /b 0\r\n",
    )
    .unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::write(
            &path,
            "#!/bin/sh\nif [ \"$1\" = metadata ]; then printf '{\"target_directory\":\"%s\"}\\n' \"$PERYX_FAKE_TARGET\"; fi\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

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
