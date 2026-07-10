use peryx_storage::blob::Digest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::cli::PrefetchPlanArgs;
use crate::config::PrefetchMode;

#[tokio::test]
async fn test_mirror_plan_expands_nested_requirements_and_trims_options() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    std::fs::write(
        dir.path().join("constraints.txt"),
        "Django==4.2 --hash=sha256:abc\n-r nested.txt\n-r constraints.txt\n# ignored\n--index-url https://example.invalid\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("nested.txt"),
        "flask[async]>=2; python_version>'3.10'\n",
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/django/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "django",
                vec![file_entry("django-4.2.tar.gz", Digest::of(b"django").as_str(), 6)],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![file_entry("flask-2.0.tar.gz", Digest::of(b"flask").as_str(), 5)],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), Vec::new());
    options.requirements.push(dir.path().join("constraints.txt"));

    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(text.contains("page\tpypi\tdjango"));
    assert!(text.contains("page\tpypi\tflask"));
}

#[tokio::test]
async fn test_mirror_plan_rejects_unsupported_selectors() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let errors = [
        "",
        "git+https://example.invalid/pkg @ main",
        "$bad",
        "not valid",
        "pkg=>1",
    ];

    for raw in errors {
        let (_text, err) = run_err(
            &mirror(&dir, &server),
            &PrefetchCommand::Plan(PrefetchPlanArgs {
                options: command_options(dir.path(), vec![raw.to_owned()]),
            }),
        )
        .await;
        assert!(err.to_string().contains("parse package selector"), "{raw}: {err}");
    }
}

#[tokio::test]
async fn test_mirror_sync_all_reads_html_project_list_and_filters_files() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let wheel = b"wheel".to_vec();
    let sdist = b"sdist".to_vec();
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            br#"<html><body><a href="/simple/flask/">Flask</a></body></html>"#.to_vec(),
            "text/html",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![
                    file_entry("flask-1.0-py3-none-any.whl", Digest::of(&wheel).as_str(), wheel.len()),
                    file_entry("flask-1.0.tar.gz", Digest::of(&sdist).as_str(), sdist.len()),
                    file_entry("flask-1.0-py3-none-any.unknown", Digest::of(b"unknown").as_str(), 7),
                    serde_json::json!({
                        "filename": "flask-1.0-missing.whl",
                        "url": "https://files.example/flask-1.0-missing.whl",
                        "hashes": {},
                    }),
                ],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), Vec::new());
    options.mode = Some(PrefetchMode::All);
    options.no_wheels = true;
    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(text.contains("flask-1.0.tar.gz"));
    assert!(text.contains("flask-1.0-py3-none-any.whl"));
    assert!(text.contains("\tskipped\twheels disabled"));
    assert!(text.contains("\tskipped\tunsupported filename"));
    assert!(text.contains("\tskipped\tmissing sha256"));
}

#[tokio::test]
async fn test_mirror_requirements_parse_errors_include_context() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let requirements = dir.path().join("requirements.txt");
    std::fs::write(&requirements, "$bad\n").unwrap();
    let mut options = command_options(dir.path(), Vec::new());
    options.requirements.push(requirements);
    let (_text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(err.to_string().contains("parse requirement"));
}

#[tokio::test]
async fn test_mirror_all_mode_errors_on_upstream_project_list_status() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), Vec::new());
    options.mode = Some(PrefetchMode::All);
    let (_text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(err.to_string().contains("upstream project list returned 503"));
}

#[tokio::test]
async fn test_mirror_selected_mode_requires_packages() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let (_text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs {
            options: command_options(dir.path(), Vec::new()),
        }),
    )
    .await;
    assert!(err.to_string().contains("has no selected packages"));
}

#[tokio::test]
async fn test_mirror_rejects_non_mirror_targets() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut config = overlay_config(dir.path(), &format!("{}/simple/", server.uri()));
    config.indexes.push(IndexConfig {
        name: "cached-two".to_owned(),
        route: "cached-two".to_owned(),
        policy: PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Cached {
            upstream: format!("{}/simple/", server.uri()),
            username: None,
            password: None,
            token: None,
            upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
            offline: false,
            prefetch: Box::default(),
        },
    });
    config.indexes.push(IndexConfig {
        name: "double".to_owned(),
        route: "double".to_owned(),
        policy: PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Virtual {
            layers: vec!["pypi".to_owned(), "cached-two".to_owned()],
            upload: None,
        },
    });
    config.indexes.push(IndexConfig {
        name: "root-virtual".to_owned(),
        route: "root-virtual".to_owned(),
        policy: PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Virtual {
            layers: vec!["hosted".to_owned()],
            upload: Some("hosted".to_owned()),
        },
    });
    let commands = [
        ("unknown", "unknown cached index"),
        ("hosted", "is hosted and has no upstream"),
        ("double", "has more than one cached member"),
        ("root-virtual", "has no cached member"),
    ];

    for (selector, expected) in commands {
        let mut options = command_options(dir.path(), vec!["flask".to_owned()]);
        options.index = selector.to_owned();
        let (_text, err) = run_err(&config, &PrefetchCommand::Plan(PrefetchPlanArgs { options })).await;
        assert!(err.to_string().contains(expected), "{selector}: {err}");
    }
}
