use std::io::ErrorKind;
use std::net::TcpListener;
use std::process::{Command, Output};

#[test]
fn shipped_binary_exposes_expected_pypi_openapi() {
    let output = Command::new(peryx_test_support::peryx_binary())
        .arg("openapi")
        .output()
        .unwrap();
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let paths = document["paths"].as_object().unwrap();

    assert_eq!(
        (
            output.status.success(),
            paths.contains_key("/{route}/simple/{project}/"),
            paths.contains_key("/v2/"),
        ),
        (true, true, std::env::var_os("PERYX_SINGLE_COMPOSITION").is_none(),),
    );
}

#[rstest::rstest]
#[case::file("[index.prefetch]\nimages = []\n", &[])]
#[case::cli("", &["--option", "images=[]"])]
fn prefetch_rejects_oci_options_before_network_access(#[case] prefetch: &str, #[case] args: &[&str]) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let output = run_prefetch(
        &format!(
            "[[index]]\nname = \"mirror\"\necosystem = \"pypi\"\n{prefetch}\
             [[index.upstream]]\nname = \"primary\"\nurl = \"http://{}\"\n",
            listener.local_addr().unwrap()
        ),
        args,
    );

    assert_eq!(
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).contains("prefetch option \"images\" is not supported by pypi"),
            output.stdout,
            listener.accept().unwrap_err().kind(),
        ),
        (false, true, Vec::new(), ErrorKind::WouldBlock)
    );
}

fn run_prefetch(config: &str, args: &[&str]) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("peryx.toml");
    std::fs::write(&config_path, config).unwrap();
    Command::new(peryx_test_support::peryx_binary())
        .args(["mirror", "plan", "mirror", "--config"])
        .arg(config_path)
        .arg("--data-dir")
        .arg(directory.path().join("data"))
        .args(args)
        .output()
        .unwrap()
}
