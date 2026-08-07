use peryx_ecosystem_pypi::store::{catalog_state, get_project, list_projects, put_project};
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

use super::*;
use crate::cli::PrefetchPlanArgs;

#[rstest]
#[case::missing_separator("packages")]
#[case::missing_key("=[\"flask\"]")]
#[case::missing_value("packages=")]
#[tokio::test]
async fn test_mirror_rejects_malformed_overrides(#[case] value: &str) {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut options = command_options(dir.path(), Vec::new());
    options.overrides.push(value.to_owned());

    let (_text, error) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;

    assert!(error.to_string().contains("must be KEY=VALUE"), "{error}");
}

#[tokio::test]
async fn test_mirror_rejects_an_invalid_override_value() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut options = command_options(dir.path(), Vec::new());
    options.overrides.push("packages=[".to_owned());

    let (_text, error) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;

    assert!(error.to_string().contains("invalid value for mirror option"), "{error}");
}

#[rstest]
#[case::array_required("packages", toml::Value::Boolean(true), "packages must be an array")]
#[case::string_entries("packages", toml::Value::Array(vec![toml::Value::Integer(1)]), "packages entries must be strings")]
#[case::boolean_required("no_wheels", toml::Value::String("yes".to_owned()), "no_wheels must be a boolean")]
#[tokio::test]
async fn test_mirror_rejects_invalid_ecosystem_options(
    #[case] key: &str,
    #[case] value: toml::Value,
    #[case] expected: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut options = command_options(dir.path(), Vec::new());
    set_option(&mut options, key, value);

    let (_text, error) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;

    assert!(error.to_string().contains(expected), "{error}");
}

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
    set_option(
        &mut options,
        "requirements",
        toml::Value::Array(vec![toml::Value::String(
            dir.path().join("constraints.txt").display().to_string(),
        )]),
    );

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
    set_option(&mut options, "mode", toml::Value::String("all".to_owned()));
    set_option(&mut options, "no_wheels", toml::Value::Boolean(true));
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
async fn test_mirror_plan_all_reuses_not_modified_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    // Key the 200-vs-304 response on the `If-None-Match` header instead of call order. The first
    // fetch sends no validator and gets the catalog with an etag; later revalidations send the etag
    // and get a 304. A retried request keeps its header, so it stays on the same branch.
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(|request: &Request| {
            if request.headers.contains_key("if-none-match") {
                ResponseTemplate::new(304)
            } else {
                ResponseTemplate::new(200)
                    .insert_header("etag", "catalog-v1")
                    .set_body_raw(
                        br#"<html><body><a href="/simple/flask/">Flask</a></body></html>"#.to_vec(),
                        "text/html",
                    )
            }
        })
        .expect(3..)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![file_entry(
                    "flask-1.0-py3-none-any.whl",
                    Digest::of(b"wheel").as_str(),
                    5,
                )],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(3..)
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), Vec::new());
    options.index = "catalog-reuse".to_owned();
    set_option(&mut options, "mode", toml::Value::String("all".to_owned()));
    let config = mirror_named(&dir, &server, "catalog-reuse");

    for _ in 0..3 {
        let text = run_ok(
            &config,
            &PrefetchCommand::Plan(PrefetchPlanArgs {
                options: options.clone(),
            }),
        )
        .await;
        assert!(text.contains("page\tcatalog-reuse\tflask"));
    }
}

#[tokio::test]
async fn test_mirror_plan_all_aborts_invalid_catalog_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    // Key the response on the `If-None-Match` header instead of call order. The first fetch sends no
    // validator and publishes `Flask` with an etag; the revalidation sends the etag and gets a
    // changed, invalid catalog that aborts the refresh. A retried request keeps its header, so it
    // stays on the same branch.
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(|request: &Request| {
            let (project, etag) = if request.headers.contains_key("if-none-match") {
                ("bad name", "catalog-v2")
            } else {
                ("Flask", "catalog-v1")
            };
            ResponseTemplate::new(200).insert_header("etag", etag).set_body_raw(
                format!(r#"{{"meta":{{"api-version":"1.4"}},"projects":[{{"name":"{project}"}}]}}"#),
                "application/vnd.pypi.simple.v1+json",
            )
        })
        .expect(2..)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                "flask",
                vec![file_entry(
                    "flask-1.0-py3-none-any.whl",
                    Digest::of(b"wheel").as_str(),
                    5,
                )],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1..)
        .mount(&server)
        .await;
    let mut options = command_options(dir.path(), Vec::new());
    options.index = "catalog-refresh".to_owned();
    set_option(&mut options, "mode", toml::Value::String("all".to_owned()));
    let config = mirror_named(&dir, &server, "catalog-refresh");
    let command = PrefetchCommand::Plan(PrefetchPlanArgs {
        options: options.clone(),
    });
    assert!(run_ok(&config, &command).await.contains("page\tcatalog-refresh\tflask"));
    let meta = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    let active_generation = catalog_state(&meta, "catalog-refresh")
        .unwrap()
        .active
        .unwrap()
        .generation;
    drop(meta);

    let (_text, error) = run_err(&config, &PrefetchCommand::Plan(PrefetchPlanArgs { options })).await;

    assert!(error.to_string().contains("bad name"));
    let meta = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    let state = catalog_state(&meta, "catalog-refresh").unwrap();
    assert_eq!(state.active.as_ref().unwrap().generation, active_generation);
    assert_eq!(state.staging, None);
    assert!(
        meta.driver_prefix_keys(&format!("pypi\0g\0catalog-refresh/{:020}/", state.next_generation))
            .unwrap()
            .is_empty()
    );
    assert_eq!(list_projects(&meta, "catalog-refresh").unwrap(), vec!["Flask"]);
    put_project(&meta, "foreground", "probe", "Probe").unwrap();
    assert_eq!(
        get_project(&meta, "foreground", "probe").unwrap().as_deref(),
        Some("Probe")
    );
}

#[tokio::test]
async fn test_mirror_requirements_parse_errors_include_context() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let requirements = dir.path().join("requirements.txt");
    std::fs::write(&requirements, "$bad\n").unwrap();
    let mut options = command_options(dir.path(), Vec::new());
    set_option(
        &mut options,
        "requirements",
        toml::Value::Array(vec![toml::Value::String(requirements.display().to_string())]),
    );
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
    set_option(&mut options, "mode", toml::Value::String("all".to_owned()));
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
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: Vec::new(),
        kind: IndexKind::Cached {
            routing: crate::tests::single_route(&format!("{}/simple/", server.uri())),
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
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: Vec::new(),
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
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: Vec::new(),
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

async fn mount_project(server: &MockServer, name: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/simple/{name}/")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_page(
                name,
                vec![file_entry(
                    &format!("{name}-1.0.tar.gz"),
                    Digest::of(name.as_bytes()).as_str(),
                    4,
                )],
            ),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_mirror_plan_accepts_pip_include_syntax() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let forms = [
        ("django", "-rattached-r.txt", "attached-r.txt"),
        ("flask", "--requirement=eq-req.txt", "eq-req.txt"),
        ("requests", "-cattached-c.txt", "attached-c.txt"),
        ("click", "--constraint=eq-con.txt", "eq-con.txt"),
        ("jinja2", "-r\ttab-r.txt", "tab-r.txt"),
        ("urllib3", "--requirement   spaced-req.txt", "spaced-req.txt"),
    ];
    let mut root = String::new();
    for (name, directive, child) in forms {
        std::fs::write(dir.path().join(child), format!("{name}\n")).unwrap();
        root.push_str(directive);
        root.push('\n');
        mount_project(&server, name).await;
    }
    std::fs::write(dir.path().join("requirements.txt"), root).unwrap();
    let mut options = command_options(dir.path(), Vec::new());
    set_option(
        &mut options,
        "requirements",
        toml::Value::Array(vec![toml::Value::String(
            dir.path().join("requirements.txt").display().to_string(),
        )]),
    );

    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    for (name, ..) in forms {
        assert!(
            text.contains(&format!("page\tpypi\t{name}")),
            "missing {name} in:\n{text}"
        );
    }
}

#[tokio::test]
async fn test_mirror_plan_skips_malformed_include_directives() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    std::fs::write(dir.path().join("child.txt"), "flask\n").unwrap();
    std::fs::write(
        dir.path().join("requirements.txt"),
        "-r child.txt\n-r\n--requirement=\n--requirements=missing.txt\n",
    )
    .unwrap();
    mount_project(&server, "flask").await;
    let mut options = command_options(dir.path(), Vec::new());
    set_option(
        &mut options,
        "requirements",
        toml::Value::Array(vec![toml::Value::String(
            dir.path().join("requirements.txt").display().to_string(),
        )]),
    );

    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(text.contains("page\tpypi\tflask"), "missing flask in:\n{text}");
}

#[tokio::test]
async fn test_mirror_requirements_join_continuations_and_strip_comments() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    std::fs::write(
        dir.path().join("requirements.txt"),
        "flask==2.3 \\\n    --hash=sha256:aaa \\\n    --hash=sha256:bbb\n\
         requests[security]==2.31\t# optional marker\n\
           # indented comment\n\
         # trailing slash comment \\\n   \nbar==1 \\\n",
    )
    .unwrap();
    for name in ["flask-2.3", "requests-2.31", "bar-1"] {
        let (project, _) = name.split_once('-').unwrap();
        Mock::given(method("GET"))
            .and(path(format!("/simple/{project}/")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                detail_page(
                    project,
                    vec![file_entry(
                        &format!("{name}.tar.gz"),
                        Digest::of(name.as_bytes()).as_str(),
                        4,
                    )],
                ),
                "application/vnd.pypi.simple.v1+json",
            ))
            .mount(&server)
            .await;
    }
    let mut options = command_options(dir.path(), Vec::new());
    set_option(
        &mut options,
        "requirements",
        toml::Value::Array(vec![toml::Value::String(
            dir.path().join("requirements.txt").display().to_string(),
        )]),
    );

    let text = run_ok(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(text.contains("page\tpypi\tflask"), "{text}");
    assert!(text.contains("page\tpypi\trequests"), "{text}");
    assert!(text.contains("page\tpypi\tbar"), "{text}");
}

#[tokio::test]
async fn test_mirror_requirements_keep_hash_glued_to_token() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let requirements = dir.path().join("requirements.txt");
    std::fs::write(&requirements, "numpy#notacomment\n").unwrap();
    let mut options = command_options(dir.path(), Vec::new());
    set_option(
        &mut options,
        "requirements",
        toml::Value::Array(vec![toml::Value::String(requirements.display().to_string())]),
    );
    let (_text, err) = run_err(
        &mirror(&dir, &server),
        &PrefetchCommand::Plan(PrefetchPlanArgs { options }),
    )
    .await;
    assert!(err.to_string().contains("numpy#notacomment"), "{err}");
}
