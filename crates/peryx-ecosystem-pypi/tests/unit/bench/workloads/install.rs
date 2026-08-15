use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use peryx_bench_core::report::load as load_report;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::super::test_support::{
    WHEEL, benchmark, http_client, install_bad_base, install_good_base, server, set_install_bases, wheel_index,
};
use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn install_workload_records_successful_and_failed_servers() {
    let good = wheel_index().await;
    let bad = MockServer::start().await;
    Mock::given(path("/simple/sample-pkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&bad)
        .await;
    set_install_bases(&good, &bad);
    let (directory, context) = benchmark();
    let servers = [server("good", install_good_base), server("bad", install_bad_base)];
    let packages = ["sample-pkg"];
    let prewarm_index = format!("{}/simple/", good.uri());

    installs_packages(
        &context,
        &servers,
        &["uv"],
        1,
        &http_client(),
        InstallInput {
            packages: &packages,
            python: "python3",
            prewarm_index: &prewarm_index,
        },
    )
    .await
    .unwrap();

    let report = load_report(&directory.path().join("report.toml")).unwrap();
    let rows = &report.tables["install-uv"].rows;
    assert_eq!(rows.len(), 4);
    for row in &rows[..2] {
        assert!(row.cells[0].value.is_some());
        assert_eq!(row.cells[1].text, "error");
    }
    let paths: Vec<_> = good
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|request| request.url.path().to_owned())
        .collect();
    let wheel_path = format!("/files/{WHEEL}");
    assert!(
        paths
            .iter()
            .filter(|path| path.as_str() == "/simple/sample-pkg/")
            .count()
            >= 3
    );
    assert!(paths.iter().filter(|path| path.as_str() == wheel_path.as_str()).count() >= 3);
}

#[tokio::test]
async fn install_workload_reports_publication_errors() {
    let (_directory, context) = benchmark();
    std::fs::write(context.report_path(), "[").unwrap();

    let error = installs_packages(
        &context,
        &[server("unused", install_good_base)],
        &["uv"],
        0,
        &http_client(),
        InstallInput {
            packages: &[],
            python: "python3",
            prewarm_index: "https://example.invalid/simple/",
        },
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "existing report is not valid TOML");
}

#[test]
fn install_plans_build_uv_and_pip_commands() {
    let venv = Path::new("/tmp/venv");
    let workdir = Path::new("/tmp/work");
    let (uv_setup, uv) = install_plan("uv", "https://index/simple/", &["one", "two"], venv, workdir);
    let (pip_setup, pip) = install_plan("pip", "https://index/simple/", &["one", "two"], venv, workdir);

    assert_eq!(uv_setup.len(), 0);
    assert_eq!(
        uv.get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("UV_CACHE_DIR".to_owned(), "/tmp/work/client-cache".to_owned()),
            ("VIRTUAL_ENV".to_owned(), "/tmp/venv".to_owned()),
        ]
    );
    assert_eq!(
        command(&uv),
        (
            "uv".to_owned(),
            vec![
                "pip",
                "install",
                "--index-url",
                "https://index/simple/",
                "--only-binary",
                ":all:",
                "one",
                "two",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )
    );
    assert_eq!(pip_setup.len(), 1);
    assert_eq!(
        command(&pip_setup[0]),
        (
            "uv".to_owned(),
            vec!["pip", "install", "--python", "/tmp/venv/bin/python", "pip"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    );
    assert_eq!(
        command(&pip),
        (
            "/tmp/venv/bin/pip".to_owned(),
            vec![
                "install",
                "--no-cache-dir",
                "--disable-pip-version-check",
                "--only-binary",
                ":all:",
                "--index-url",
                "https://index/simple/",
                "one",
                "two",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )
    );
}

#[test]
fn install_plan_runner_reports_process_failures() {
    let directory = tempfile::tempdir().unwrap();
    let setup_path = directory.path().join("setup");
    let install_path = directory.path().join("install");
    let mut setup = Command::new("touch");
    setup.arg(&setup_path);
    let mut install = Command::new("touch");
    install.arg(&install_path);
    run_install_plan("https://index/simple/", vec![setup], install).unwrap();
    assert_eq!((setup_path.exists(), install_path.exists()), (true, true));
    assert_eq!(
        run_install_plan("https://index/simple/", Vec::new(), Command::new("false"))
            .unwrap_err()
            .to_string(),
        "install via https://index/simple/ failed:\n"
    );
    assert_eq!(
        run_install_plan(
            "https://index/simple/",
            Vec::new(),
            Command::new("peryx-missing-install"),
        )
        .unwrap_err()
        .to_string(),
        "install client did not start"
    );
}

fn command(command: &Command) -> (String, Vec<String>) {
    (
        command.get_program().to_string_lossy().into_owned(),
        command
            .get_args()
            .map(OsStr::to_string_lossy)
            .map(std::borrow::Cow::into_owned)
            .collect(),
    )
}
