use std::ffi::OsStr;

use peryx_bench_core::context::BenchmarkContext;

use super::*;

type Target = (
    &'static str,
    &'static str,
    String,
    String,
    Option<String>,
    Option<Vec<String>>,
);

#[test]
fn server_targets_build_expected_urls_and_commands() {
    temp_env::with_var(UVX_ENV, None::<&str>, || {
        let directory = tempfile::tempdir().unwrap();
        let context = BenchmarkContext::new("peryx-bin".into(), directory.path().join("report.toml"));
        let state = directory.path().join("state");
        std::fs::create_dir(&state).unwrap();
        assert_eq!(targets(&context, &state), expected_targets(&state));
    });
}

fn targets(context: &BenchmarkContext, state: &std::path::Path) -> Vec<Target> {
    all()
        .iter()
        .map(|server| {
            let base = (server.base_url)(4321);
            let command = server.command.map(|build| build(context, 4321, state));
            (
                server.name,
                server.homepage,
                base.clone(),
                (server.probe)(&base),
                command
                    .as_ref()
                    .map(|command| command.get_program().to_string_lossy().into_owned()),
                command.as_ref().map(|command| {
                    command
                        .get_args()
                        .map(OsStr::to_string_lossy)
                        .map(std::borrow::Cow::into_owned)
                        .collect::<Vec<_>>()
                }),
            )
        })
        .collect()
}

fn expected_targets(state: &std::path::Path) -> Vec<Target> {
    expected_core_targets(state)
        .into_iter()
        .chain(expected_competitor_targets(state))
        .collect()
}

fn expected_core_targets(state: &std::path::Path) -> Vec<Target> {
    let state = state.display().to_string();
    vec![
        (
            "peryx",
            "https://peryx.readthedocs.io/",
            "http://127.0.0.1:4321/root/pypi/simple/".to_owned(),
            "http://127.0.0.1:4321/root/pypi/simple/six/".to_owned(),
            Some("peryx-bin".to_owned()),
            Some(strings(&[
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                "4321",
                "--data-dir",
                &state,
            ])),
        ),
        (
            "direct",
            "https://pypi.org/",
            "https://pypi.org/simple/".to_owned(),
            "https://pypi.org/simple/six/".to_owned(),
            None,
            None,
        ),
        (
            "devpi",
            "https://devpi.net/docs/",
            "http://127.0.0.1:4321/root/pypi/+simple/".to_owned(),
            "http://127.0.0.1:4321/root/pypi/+simple/six/".to_owned(),
            Some("uvx".to_owned()),
            Some(strings(&[
                "--from",
                "devpi-server",
                "devpi-server",
                "--serverdir",
                &state,
                "--port",
                "4321",
            ])),
        ),
    ]
}

fn expected_competitor_targets(state: &std::path::Path) -> Vec<Target> {
    let state = state.display().to_string();
    let config = format!("{state}/pypicloud.ini");
    vec![
        (
            "proxpi",
            "https://github.com/EpicWink/proxpi",
            "http://127.0.0.1:4321/index/".to_owned(),
            "http://127.0.0.1:4321/index/six/".to_owned(),
            Some("uvx".to_owned()),
            Some(strings(&[
                "--from",
                "proxpi",
                "--with",
                "gunicorn",
                "gunicorn",
                "--bind",
                "127.0.0.1:4321",
                "--workers",
                "4",
                "proxpi.server:app",
            ])),
        ),
        (
            "pypiserver",
            "https://github.com/pypiserver/pypiserver",
            "http://127.0.0.1:4321/simple/".to_owned(),
            "http://127.0.0.1:4321/simple/six/".to_owned(),
            Some("uvx".to_owned()),
            Some(strings(&[
                "--from",
                "pypiserver[passlib]",
                "pypi-server",
                "run",
                "-p",
                "4321",
                "--fallback-url",
                "https://pypi.org/simple/",
                "-P",
                ".",
                "-a",
                ".",
                &state,
            ])),
        ),
        (
            "pypicloud",
            "https://pypicloud.readthedocs.io/",
            "http://127.0.0.1:4321/simple/".to_owned(),
            "http://127.0.0.1:4321/simple/six/".to_owned(),
            Some("uvx".to_owned()),
            Some(strings(&[
                "--python",
                "3.10",
                "--from",
                "pypicloud",
                "--with",
                "sqlalchemy<2",
                "--with",
                "waitress",
                "pserve",
                &config,
            ])),
        ),
    ]
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}

#[test]
fn server_setup_hooks_create_expected_state() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path();
    let servers = all();
    temp_env::with_var(UVX_ENV, Some("true"), || {
        (servers[2].setup.unwrap())(4321, state).unwrap();
    });
    (servers[5].setup.unwrap())(4321, state).unwrap();

    let ini = std::fs::read_to_string(state.join("pypicloud.ini")).unwrap();
    assert_eq!(
        ini,
        format!(
            "[app:main]\n\
             use = egg:pypicloud\n\
             pyramid.reload_templates = False\n\
             pypi.fallback = cache\n\
             pypi.default_read = everyone\n\
             pypi.cache_update = everyone\n\
             pypi.storage = file\n\
             storage.dir = {}\n\
             db.url = sqlite:///{}\n\
             session.encrypt_key = {}\n\
             session.validate_key = {}\n\
             auth.admins =\n\
             \n\
             [server:main]\n\
             use = egg:waitress#main\n\
             host = 127.0.0.1\n\
             port = 4321\n\
             threads = 8\n",
            state.join("packages").display(),
            state.join("db.sqlite").display(),
            "0".repeat(64),
            "0".repeat(64),
        )
    );

    let output = Command::new("false").output().unwrap();
    assert_eq!(
        check_devpi_init(&output).unwrap_err().to_string(),
        "devpi-init failed:\n"
    );
}
