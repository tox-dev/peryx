use std::process::{Command, Output};

#[test]
fn shipped_binary_exposes_expected_oci_openapi() {
    let output = Command::new(peryx_test_support::peryx_binary())
        .arg("openapi")
        .output()
        .unwrap();
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let paths = document["paths"].as_object().unwrap();

    assert_eq!(
        (
            output.status.success(),
            paths.contains_key("/v2/"),
            paths.contains_key("/{route}/simple/{project}/"),
        ),
        (true, true, std::env::var_os("PERYX_SINGLE_COMPOSITION").is_none(),),
    );
}

#[rstest::rstest]
#[case::requirements("requirements=[]", "requirements")]
#[case::mode("mode=\"all\"", "mode")]
#[case::python_tags("python_tags=[]", "python_tags")]
#[case::metadata_only("metadata_only=true", "metadata_only")]
fn prefetch_rejects_pypi_cli_options(#[case] option: &str, #[case] key: &str) {
    let output = run_prefetch("", &["--option", option]);

    assert_eq!(
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("prefetch option {key:?} is not supported by oci")),
            output.stdout,
        ),
        (false, true, Vec::new())
    );
}

#[rstest::rstest]
#[case::file_packages("[index.prefetch]\npackages = [\"library/example:latest\"]\n", &[])]
#[case::cli_images("", &["--option", "images=[\"library/example:latest\"]"])]
fn prefetch_preserves_valid_selections(#[case] prefetch: &str, #[case] args: &[&str]) {
    let output = run_prefetch(prefetch, args);

    assert_eq!(
        (
            output.status.success(),
            String::from_utf8(output.stdout).unwrap(),
            output.stderr,
        ),
        (
            true,
            "kind\tindex\tproject\tfilename\tdigest\turl\tbytes\tstatus\treason\n\
             manifest\tmirror\tlibrary/example\tlatest\t\t\t0\tselected\t\n\
             summary\tmirror\t\timages\t\t\t1\timages\t\n"
                .to_owned(),
            Vec::new(),
        )
    );
}

fn run_prefetch(prefetch: &str, args: &[&str]) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("peryx.toml");
    std::fs::write(
        &config_path,
        format!(
            "[[index]]\nname = \"mirror\"\necosystem = \"oci\"\n{prefetch}\
             [[index.upstream]]\nname = \"primary\"\nurl = \"http://127.0.0.1:1\"\n"
        ),
    )
    .unwrap();
    Command::new(peryx_test_support::peryx_binary())
        .args(["mirror", "plan", "mirror", "--config"])
        .arg(config_path)
        .arg("--data-dir")
        .arg(directory.path().join("data"))
        .args(args)
        .output()
        .unwrap()
}
