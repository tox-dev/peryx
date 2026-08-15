#[cfg(unix)]
use std::io::Write as _;
use std::path::Path;

use clap::Command;
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::suite::{BenchmarkRun, BenchmarkSuite as _};
use rstest::rstest;
#[cfg(unix)]
use wiremock::matchers::{header, method, path, query_param};
#[cfg(unix)]
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

#[test]
fn suite_configures_oci_options() {
    let matches = SUITE
        .configure(Command::new("bench"))
        .try_get_matches_from(["bench", "--mirror"])
        .unwrap();
    assert_eq!((SUITE.name(), matches.get_flag(MIRROR_ARG)), ("oci", true));
}

#[tokio::test]
async fn suite_accepts_an_empty_workload_selection() {
    let directory = tempfile::tempdir().unwrap();
    let matches = SUITE.configure(Command::new("bench")).get_matches_from(["bench"]);
    let skip = ["pull", "throughput", "parallel", "endpoints"].map(str::to_owned);
    SUITE
        .run(BenchmarkRun {
            context: &context(directory.path()),
            rounds: 1,
            skip: &skip,
            only: "",
            http: &peryx_bench_core::servers::http_client().unwrap(),
            matches: &matches,
        })
        .await
        .unwrap();
}

#[rstest]
#[case("https://index.docker.io/library/alpine", "https://index.docker.io/v2/")]
#[case("localhost:5000/root", "localhost:5000/root/v2/")]
fn api_root_normalizes_registry_bases(#[case] base: &str, #[case] expected: &str) {
    assert_eq!(servers::api_root(base), expected);
}

#[rstest]
#[case("https://registry.test/root/", "repo:tag", "registry.test/root/repo:tag", false)]
#[case("http://registry.test", "repo", "registry.test/repo", true)]
#[case("registry.test/", "repo", "registry.test/repo", false)]
fn client_reference_and_transport(
    #[case] base: &str,
    #[case] image: &str,
    #[case] expected: &str,
    #[case] insecure: bool,
) {
    assert_eq!(
        (servers::client_reference(base, image), servers::insecure(base)),
        (expected.to_owned(), insecure)
    );
}

#[cfg(unix)]
#[test]
fn server_catalog_exposes_each_competitor() {
    let parties = servers::all();
    assert_eq!(
        parties.iter().map(|server| server.name).collect::<Vec<_>>(),
        ["peryx", "direct", "distribution", "zot"]
    );
    assert_eq!((servers::reports(&parties)[1].base_url)(0), servers::DOCKERHUB);
}

#[cfg(unix)]
#[test]
fn server_environment_selects_direct_and_mirrored_upstreams() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    let anonymous = tools.environment(None);
    let authenticated = tools.environment(Some(("user".to_owned(), "token".to_owned())));

    assert_eq!(
        (
            servers::upstream_for(&anonymous, "https://hub"),
            servers::table_name(&anonymous, "pull"),
            servers::hub_credentials(&anonymous),
        ),
        ("https://hub".to_owned(), "pull".to_owned(), None)
    );
    let mirrored = authenticated.with_mirror("http://127.0.0.1:5000".to_owned());
    assert_eq!(
        (
            servers::upstream_for(&mirrored, "https://hub"),
            servers::table_name(&mirrored, "pull"),
            servers::hub_credentials(&mirrored),
        ),
        ("http://127.0.0.1:5000".to_owned(), "pull-mirror".to_owned(), None)
    );
    servers::login_crane(&mirrored).unwrap();
}

#[cfg(unix)]
#[test]
fn server_login_reports_rejected_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    tools.set_mode("auth-fail");

    assert!(servers::login_crane(&tools.environment(Some(("user".to_owned(), "token".to_owned())))).is_err());
    servers::login_crane(&tools.environment(None)).unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn peryx_server_writes_oci_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    let peryx = servers::all().remove(0);

    assert!(
        peryx
            .start(
                &tools.environment(Some(("user".to_owned(), "token".to_owned()))),
                &context(directory.path()),
                directory.path(),
            )
            .await
            .is_err()
    );
    let config = std::fs::read_to_string(directory.path().join("peryx.toml")).unwrap();
    assert!(config.contains("ecosystem = \"oci\"") && config.contains("username = \"user\""));
}

#[cfg(unix)]
#[rstest]
#[case(None, false, "did not start")]
#[case(
    Some("#!/bin/sh\nexec python3 -c 'import signal; signal.pause()'\n"),
    true,
    "did not emit its startup event"
)]
#[case(Some("#!/bin/sh\nexit 7\n"), false, "exited before its startup event")]
#[case(
    Some("#!/bin/sh\nprintf 'peryx listening\\n'\nexit 7\n"),
    false,
    "exited before OCI pulls were ready"
)]
#[case(
    Some("#!/bin/sh\nprintf 'peryx listening\\n'\nexec python3 -c 'import signal; signal.pause()'\n"),
    false,
    "could not pull through"
)]
#[case(Some(ONE_SHOT_REGISTRY), false, "exited after OCI pull readiness")]
#[tokio::test(flavor = "current_thread")]
async fn peryx_server_classifies_startup_failures(
    #[case] script: Option<&str>,
    #[case] immediate_deadline: bool,
    #[case] expected: &str,
) {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    if let Some(script) = script {
        write_tool(&directory.path().join("peryx"), script);
    }
    let mut environment = tools.environment(Some(("user".to_owned(), "token".to_owned())));
    if immediate_deadline {
        environment.startup_timeout = std::time::Duration::ZERO;
    }

    assert!(
        servers::all()
            .remove(0)
            .start(&environment, &context(directory.path()), directory.path())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains(expected)
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn direct_server_uses_the_upstream_without_a_process() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());

    let active = servers::all()
        .remove(1)
        .start(
            &tools.environment(Some(("user".to_owned(), "token".to_owned()))),
            &context(directory.path()),
            directory.path(),
        )
        .await
        .unwrap();
    assert_eq!(active.url, servers::DOCKERHUB);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn distribution_server_reaps_its_fixture_process() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    let active = servers::all()
        .remove(2)
        .start(
            &tools.environment(Some(("user".to_owned(), "token".to_owned()))),
            &context(directory.path()),
            directory.path(),
        )
        .await
        .unwrap();
    assert!(active.url.starts_with("http://127.0.0.1:"));
    assert!(
        std::fs::read_to_string(tools.directory.join("crane.calls"))
            .unwrap()
            .lines()
            .any(|call| call.contains("pull --insecure") && call.contains("library/hello-world:latest"))
    );
    let pid = std::fs::read_to_string(tools.directory.join("distribution.pid")).unwrap();

    drop(active);

    assert!(
        !std::process::Command::new("kill")
            .args(["-0", &pid])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn zot_server_writes_authenticated_sync_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    let environment = tools.environment(Some(("user".to_owned(), "token".to_owned())));
    let zot_binary = environment.cache.join("zot");
    std::fs::create_dir_all(zot_binary.parent().unwrap()).unwrap();
    std::fs::write(&zot_binary, "fixture").unwrap();

    assert!(
        servers::all()
            .remove(3)
            .start(&environment, &context(directory.path()), directory.path())
            .await
            .is_err()
    );
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("zot.json")).unwrap()).unwrap();
    assert!(config["http"]["port"].as_str().is_some());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(config["extensions"]["sync"]["credentialsFile"].as_str().unwrap()).unwrap(),
        )
        .unwrap(),
        serde_json::json!({"registry-1.docker.io": {"username": "user", "password": "token"}})
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn zot_server_reports_an_unwritable_state_directory() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    let environment = tools.environment(None);
    let zot_binary = environment.cache.join("zot");
    std::fs::create_dir_all(zot_binary.parent().unwrap()).unwrap();
    std::fs::write(zot_binary, "fixture").unwrap();

    assert!(
        servers::all()
            .remove(3)
            .start(
                &environment,
                &context(directory.path()),
                &directory.path().join("missing"),
            )
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[rstest]
#[case("curl-success", true)]
#[case("curl-fail", false)]
#[tokio::test(flavor = "current_thread")]
async fn zot_server_reports_download_outcomes(#[case] mode: &str, #[case] downloaded: bool) {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    tools.set_mode(mode);
    let environment = tools.environment(Some(("user".to_owned(), "token".to_owned())));
    let zot_binary = environment.cache.join("zot");

    assert!(
        servers::all()
            .remove(3)
            .start(&environment, &context(directory.path()), directory.path())
            .await
            .is_err()
    );
    assert_eq!(zot_binary.exists(), downloaded);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn mirror_boundaries_cover_success_and_failure() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    mirror_contract(&tools).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn workload_boundaries_cover_success_and_failure() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    let registry = registry_server().await;
    workload_contract(
        directory.path(),
        &tools,
        &tools
            .environment(Some(("user".to_owned(), "token".to_owned())))
            .with_mirror(registry.uri()),
        &peryx_bench_core::servers::http_client().unwrap(),
    )
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn suite_boundaries_cover_success_and_failure() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::install(directory.path());
    suite_contract(
        directory.path(),
        &tools,
        &peryx_bench_core::servers::http_client().unwrap(),
    )
    .await;
}

#[cfg(unix)]
async fn mirror_contract(tools: &Tools) {
    tools.set_mode("simple");
    let environment = tools.environment(None);
    let mirror = servers::start_mirror(&environment).await.unwrap();
    assert!(environment.with_mirror(mirror.url().to_owned()).mirror.is_some());
    drop(mirror);

    tools.set_mode("docker-fail");
    assert!(servers::start_mirror(&environment).await.is_err());
    tools.set_mode("pull-fail");
    assert!(servers::start_mirror(&environment).await.is_err());
    tools.set_mode("require-auth");
    assert!(servers::start_mirror(&environment).await.is_err());
    drop(
        servers::start_mirror(&tools.environment(Some(("user".to_owned(), "token".to_owned()))))
            .await
            .unwrap(),
    );
}

#[cfg(unix)]
async fn workload_contract(state: &Path, tools: &Tools, environment: &BenchEnvironment, http: &reqwest::Client) {
    tools.set_mode("simple");
    let context = context(state);
    let servers = [servers::all().remove(1)];
    workloads::pulls(environment, &context, &servers, 1).await.unwrap();
    workloads::throughput(environment, &context, &servers, 1, http)
        .await
        .unwrap();
    workloads::fleet(environment, &context, &servers, 1).await.unwrap();
    workloads::endpoints(environment, &context, &servers, 1, http)
        .await
        .unwrap();
    tools.set_mode("index");
    workloads::throughput(environment, &context, &servers, 1, http)
        .await
        .unwrap();
    tools.set_mode("index-missing");
    assert!(
        workloads::throughput(environment, &context, &servers, 1, http)
            .await
            .is_err()
    );
    let report = std::fs::read_to_string(context.report_path()).unwrap();
    assert!(
        report.contains("pull")
            && report.contains("image-throughput")
            && report.contains("parallel-pull")
            && report.contains("image-endpoints")
    );

    tools.set_mode("pull-fail");
    workloads::pulls(environment, &context, &servers, 1).await.unwrap();
    workloads::fleet(environment, &context, &servers, 1).await.unwrap();
    workloads::endpoints(environment, &context, &servers, 1, http)
        .await
        .unwrap();
    tools.set_mode("manifest-fail");
    assert!(
        workloads::throughput(environment, &context, &servers, 1, http)
            .await
            .is_err()
    );
    tools.set_mode("manifest-invalid");
    assert!(
        workloads::throughput(environment, &context, &servers, 1, http)
            .await
            .is_err()
    );
}

#[cfg(unix)]
async fn suite_contract(state: &Path, tools: &Tools, http: &reqwest::Client) {
    tools.set_mode("simple");
    let context = context(state);
    assert!(
        run_suite(tools.environment(None), &context, false, 1, &[], "absent", http)
            .await
            .is_err()
    );
    run_suite(tools.environment(None), &context, true, 1, &[], "direct", http)
        .await
        .unwrap();
}

#[cfg(unix)]
async fn registry_server() -> MockServer {
    let server = MockServer::start().await;
    let manifest = serde_json::json!({
        "config": {"digest": "sha256:config"},
        "layers": [{"digest": "sha256:layer", "size": 4}]
    });
    for route in [
        "/v2/library/python/manifests/3.12-slim",
        "/v2/library/python/manifests/sha256:manifest",
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("docker-content-digest", "sha256:manifest")
                    .set_body_json(manifest.clone()),
            )
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
    }
    for route in [
        "/v2/library/python/blobs/sha256:config",
        "/v2/library/python/blobs/sha256:layer",
        "/v2/",
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"blob"))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/v2/library/python/tags/list"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "library/python", "tags": []})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/python/tags/list"))
        .and(query_param("n", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "library/python", "tags": []})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(header("accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"token": "token"})))
        .mount(&server)
        .await;
    server
}

fn context(state: &Path) -> BenchmarkContext {
    BenchmarkContext::new(state.join("peryx"), state.join("report.toml"))
}

#[cfg(unix)]
struct Tools {
    directory: std::path::PathBuf,
}

#[cfg(unix)]
impl Tools {
    fn install(root: &Path) -> Self {
        let directory = root.join("bin");
        std::fs::create_dir(&directory).unwrap();
        let tools = Self { directory };
        tools.set_mode("simple");
        tools
    }

    fn environment(&self, credentials: Option<(String, String)>) -> BenchEnvironment {
        BenchEnvironment::new(Some(&self.directory), credentials)
    }

    fn set_mode(&self, mode: &str) {
        write_tool(&self.directory.join("crane"), &CRANE_TOOL.replace("__MODE__", mode));
        write_tool(&self.directory.join("docker"), &DOCKER_TOOL.replace("__MODE__", mode));
        write_tool(&self.directory.join("curl"), &CURL_TOOL.replace("__MODE__", mode));
    }
}

#[cfg(unix)]
fn write_tool(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    let mut permissions = file.metadata().unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(unix)]
const CRANE_TOOL: &str = r#"#!/bin/sh
mode=__MODE__
printf '%s\n' "$*" >>"${0%/*}/crane.calls"
case "$1" in
  auth)
    cat >/dev/null
    [ "$mode" = auth-fail ] && exit 1
    exit 0
    ;;
  pull)
    [ "$mode" = pull-fail ] && { echo pull-failed >&2; exit 1; }
    shift
    [ "$1" = --insecure ] && shift
    reference=$1
    for argument in "$@"; do destination=$argument; done
    host=${reference%%/*}
    python3 - "$host" <<'PY' || exit 1
import http.client
import sys
connection = http.client.HTTPConnection(sys.argv[1])
connection.request("GET", "/v2/")
connection.getresponse().read()
PY
    printf image >"$destination"
    ;;
  manifest)
    [ "$mode" = manifest-fail ] && { echo manifest-failed >&2; exit 1; }
    [ "$mode" = manifest-invalid ] && { printf invalid; exit 0; }
    target=$2
    [ "$target" = --insecure ] && target=$3
    if { [ "$mode" = index ] || [ "$mode" = index-missing ]; } && ! printf %s "$target" | grep -q @; then
      if [ "$mode" = index-missing ]; then architecture=missing; else architecture=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/'); fi
      printf '{"manifests":[{"digest":"sha256:child","platform":{"architecture":"%s","os":"linux"}}]}' "$architecture"
    else
      printf '{"config":{"digest":"sha256:config"},"layers":[{"digest":"sha256:layer","size":4}]}'
    fi
    ;;
esac
"#;

#[cfg(unix)]
const DOCKER_TOOL: &str = r#"#!/bin/sh
mode=__MODE__
[ "$mode" = docker-fail ] && { echo docker-failed >&2; exit 1; }
if [ "$1" = run ]; then
  if [ "$mode" = require-auth ]; then
    case "$*" in
      *REGISTRY_PROXY_USERNAME=user*REGISTRY_PROXY_PASSWORD=token*) ;;
      *) exit 1 ;;
    esac
  fi
  detached=false
  while [ "$#" -gt 0 ]; do
    [ "$1" = -d ] && detached=true
    if [ "$1" = -p ]; then mapping=$2; break; fi
    shift
  done
  port=$(printf %s "$mapping" | cut -d: -f2)
  if [ "$detached" = true ]; then
    ready=${TMPDIR:-/tmp}/peryx-oci-ready-$port
    pidfile=${TMPDIR:-/tmp}/peryx-oci-pid-$port
    rm -f "$ready"
    mkfifo "$ready"
    python3 -c 'import http.server,sys; s=http.server.HTTPServer(("127.0.0.1",int(sys.argv[1])),http.server.SimpleHTTPRequestHandler); open(sys.argv[2],"w").write("ready\n"); s.serve_forever()' "$port" "$ready" >/dev/null 2>&1 &
    printf %s $! >"$pidfile"
    IFS= read -r signal <"$ready"
    rm -f "$ready"
    [ "$signal" = ready ]
    printf 'listening on fixture\n' >&2
    printf fixture
  else
    exec python3 - "$port" "${0%/*}/distribution.pid" <<'PY'
import http.server
import os
import sys

open(sys.argv[2], "w").write(str(os.getpid()))
print("listening on fixture", file=sys.stderr, flush=True)
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), http.server.SimpleHTTPRequestHandler).serve_forever()
PY
  fi
elif [ "$1" = logs ]; then
  printf 'listening on fixture\n'
elif [ "$1" = rm ]; then
  name=$3
  port=${name##*-}
  pidfile=${TMPDIR:-/tmp}/peryx-oci-pid-$port
  [ -e "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true
  rm -f "$pidfile"
fi
"#;

#[cfg(unix)]
const CURL_TOOL: &str = r#"#!/bin/sh
mode=__MODE__
[ "$mode" != curl-success ] && exit 1
while [ "$#" -gt 0 ]; do
  if [ "$1" = --output ]; then output=$2; break; fi
  shift
done
printf '#!/bin/sh\n' >"$output"
"#;

#[cfg(unix)]
const ONE_SHOT_REGISTRY: &str = r#"#!/bin/sh
port=$(sed -n 's/^port = //p' "$3")
python3 - "$port" <<'PY'
import http.server
import os
import sys

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ready")
        self.wfile.flush()
        os._exit(0)

    def log_message(self, _format, *_args):
        pass

server = http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
print("peryx listening", flush=True)
server.handle_request()
PY
"#;
