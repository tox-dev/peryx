#![cfg(feature = "system-tests")]

use std::ffi::OsStr;
use std::process::{Command, Output};

fn run(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    Command::new(peryx_test_support::cargo_binary("peryx"))
        .args(args)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn oci_policy_reference_keys_pass_config_check() {
    let policy: String = include_str!("../../../site/content/ecosystems/oci/reference/policy.md")
        .lines()
        .filter_map(|line| {
            let key = line.strip_prefix("| `")?.split_once('`')?.0;
            let value = match key {
                "allow_resources" | "block_resources" | "protected_resources" => "[\"team/api\"]",
                "quota_audit" => "true",
                _ => "1",
            };
            Some(format!("{key} = {value}\n"))
        })
        .collect();
    assert!(policy.contains("max_tags_per_repository = 1\n"));
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    assert_success(&run([OsStr::new("init"), OsStr::new("--data-dir"), data.as_os_str()]));
    let config = directory.path().join("peryx.toml");
    std::fs::write(
        &config,
        format!("[[index]]\nname = \"images\"\necosystem = \"oci\"\nhosted = true\n\n[index.policy]\n{policy}"),
    )
    .unwrap();

    assert_success(&run([
        OsStr::new("config"),
        OsStr::new("check"),
        OsStr::new("--config"),
        config.as_os_str(),
        OsStr::new("--data-dir"),
        data.as_os_str(),
    ]));
}
