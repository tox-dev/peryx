use std::cell::Cell;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::os::{
    fd::{AsFd as _, OwnedFd},
    unix::net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use peryx_core::Ecosystem;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{
    AbsoluteProtocolDriver, CapabilityRegistrar, ClientDiscovery, EcosystemDriver, NameDriver,
};
use peryx_driver::state::{AppState, IndexDescription};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use peryx_test_support::{
    ADMIN_PASSWORD, ADMIN_USER, Cluster, EcosystemDriverFixture, HarnessError, MemberSpec, Node, OwnershipControl,
    ProcessHarness, ProcessLimit, Role, Topology, Toxiproxy, cargo_binary, process_alive, reachable_through,
};
use tempfile::TempDir;

const FAILURE_TIMEOUT: Duration = Duration::from_millis(100);
const TOXIPROXY_FAILURE_TIMEOUT: Duration = Duration::from_secs(2);
const FIXTURE_DEADLOCK_GUARD: Duration = Duration::from_secs(90);
const FIXTURE_ECOSYSTEM: Ecosystem = Ecosystem::new("fixture");
const OTHER_ECOSYSTEM: Ecosystem = Ecosystem::new("other");

static FIXTURE_DRIVER: EcosystemDriverFixture = EcosystemDriverFixture::new(FIXTURE_ECOSYSTEM, RouteClass::Artifact);
struct NameFixture;

impl NameDriver for NameFixture {
    fn normalize_name(&self, name: &str) -> String {
        name.to_ascii_lowercase()
    }
}

fn fixture_capabilities(registrar: &mut dyn CapabilityRegistrar) {
    registrar.register_name(OTHER_ECOSYSTEM, Arc::new(NameFixture));
}

#[test]
fn ecosystem_driver_fixture_registers_declared_behavior() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
    );
    EcosystemDriverFixture::new(FIXTURE_ECOSYSTEM, RouteClass::Artifact).register(&mut state);
    FIXTURE_DRIVER.register_with_discovery(&mut state);
    let configured =
        EcosystemDriverFixture::new(OTHER_ECOSYSTEM, RouteClass::Listing).with_capabilities(fixture_capabilities);
    configured.register(&mut state);
    let description = IndexDescription {
        name: "fixture".to_owned(),
        route: "fixture".to_owned(),
        ecosystem: "fixture".to_owned(),
        kind: "hosted",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: None,
        hosted: None,
    };

    assert_eq!(FIXTURE_DRIVER.ecosystem().as_str(), FIXTURE_ECOSYSTEM.as_str());
    assert_eq!(FIXTURE_DRIVER.classify_route("/artifact"), RouteClass::Artifact);
    assert_eq!(
        state
            .driver_set()
            .get_name(&OTHER_ECOSYSTEM)
            .unwrap()
            .normalize_name("MiXeD"),
        "mixed"
    );
    assert_eq!(
        FIXTURE_DRIVER.discover_index(description.clone(), None),
        peryx_driver::discovery::minimal_entry(&description)
    );
    assert_eq!(FIXTURE_DRIVER.client_endpoint("fixture"), "/fixture/");
    assert_eq!(FIXTURE_DRIVER.prefixes(), &["/__fixture/"]);
    assert!(state.driver_for(&FIXTURE_ECOSYSTEM).is_some());
    assert!(state.driver_for(&OTHER_ECOSYSTEM).is_some());
    assert!(state.client_discovery_for(&FIXTURE_ECOSYSTEM).is_some());
}

#[tokio::test]
async fn ecosystem_driver_fixture_rejects_protocol_requests() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
    );

    assert_eq!(
        FIXTURE_DRIVER
            .serve(Arc::clone(&state.serving), Request::new(Body::empty()))
            .await
            .status(),
        StatusCode::NOT_FOUND,
    );
}

#[test]
fn harness_errors_preserve_diagnostic_context() {
    let messages = [
        HarnessError::Io(std::io::Error::other("disk")).to_string(),
        HarnessError::Config("bad config".to_owned()).to_string(),
        HarnessError::NoTransfer {
            from: "dc-a".to_owned(),
            within: Duration::ZERO,
            observed: Some("dc-a".to_owned()),
        }
        .to_string(),
        HarnessError::NoLeader { within: Duration::ZERO }.to_string(),
        HarnessError::SignalTimeout {
            node: "node-a".to_owned(),
            within: Duration::ZERO,
            last: "pending".to_owned(),
        }
        .to_string(),
        HarnessError::SignalClosed {
            node: "node-a".to_owned(),
            last: "pending".to_owned(),
        }
        .to_string(),
        HarnessError::SignalRead {
            node: "node-a".to_owned(),
            failure: "broken stream".to_owned(),
            last: "pending".to_owned(),
        }
        .to_string(),
        HarnessError::NotReady {
            node: "node-a".to_owned(),
            timeout: Duration::ZERO,
            log: "tail".to_owned(),
        }
        .to_string(),
        HarnessError::ExitedEarly {
            node: "node-a".to_owned(),
            status: "1".to_owned(),
            log: "tail".to_owned(),
        }
        .to_string(),
        HarnessError::Toxiproxy("down".to_owned()).to_string(),
    ];
    assert_eq!(
        messages,
        [
            "I/O: disk",
            "peryx rejected the generated config:\nbad config",
            "authority did not leave \"dc-a\" within 0ns; it still reports Some(\"dc-a\")",
            "no authority leader emerged within 0ns",
            "node \"node-a\" emitted no matching topology signal within 0ns; last observation: pending",
            "node \"node-a\" closed its topology stream; last observation: pending",
            "node \"node-a\" topology stream failed: broken stream; last observation: pending",
            "node \"node-a\" did not become ready within 0ns\n--- log tail ---\ntail",
            "node \"node-a\" exited during startup with 1\n--- log tail ---\ntail",
            "toxiproxy: down",
        ]
    );
}

#[test]
fn generated_topology_configs_pass_peryx_validation() {
    with_fixture(|fixture| {
        let single_config = fixture
            .topology(Topology::single())
            .with_index_config("[[index]]\nname = \"sample\"")
            .validate_config()
            .expect("validate single config");
        assert!(single_config.contains("name = \"sample\""));
        assert!(!single_config.contains("[availability]"));

        let members = members();
        let dc_config = fixture
            .topology(Topology::dc("group-a", members.clone()))
            .with_write_ack_deadline(3)
            .with_replication_token("custom-token")
            .validate_config()
            .expect("validate dc config");
        assert!(dc_config.contains("mode = \"dc\""));
        assert!(dc_config.contains("deadline-secs = 3"));
        assert_eq!(dc_config.matches("token = \"custom-token\"").count(), 1);
        assert!(dc_config.contains("role = \"writer\""));
        assert!(dc_config.contains("role = \"replica\""));

        let ha_config = fixture
            .topology(Topology::ha("group-a", members))
            .validate_config()
            .expect("validate ha config");
        assert!(ha_config.contains("mode = \"ha\""));
    });
}

#[test]
fn node_public_behavior() {
    with_fixture(|fixture| {
        let mut cluster = start_dc_cluster(fixture);
        assert_eq!(cluster.nodes().len(), 2);
        assert_eq!(cluster.node("writer").map(Node::identity), Some("writer"),);
        assert!(cluster.node("missing").is_none());
        assert_eq!(cluster.leader().expect("read leader"), Some("dc-a".to_owned()));
        assert_eq!(cluster.await_leader(Duration::ZERO).expect("await leader"), "dc-a");

        let writer = &mut cluster.nodes_mut()[0];
        assert_eq!(writer.identity(), "writer");
        assert!(writer.port() > 0);
        assert!(writer.control_endpoint().starts_with("127.0.0.1:"));
        assert!(writer.pid() > 0);
        assert!(process_alive(writer.pid()));
        assert!(writer.is_running());
        assert!(writer.is_ready());
        assert_eq!(writer.status(), Some((200, "{\"version\":\"test\"}".to_owned())));
        assert_eq!(writer.readiness(), Some((200, "ready".to_owned())));
        assert_eq!(writer.topology(), Some((200, "topology".to_owned())));
        assert_eq!(writer.consensus_leader(), Some("dc-a".to_owned()));
        assert_eq!(writer.metrics(), Some((200, "metric 1\n".to_owned())));
        assert_eq!(writer.placements(), Some((200, "placements".to_owned())));
        assert_eq!(writer.http_get("/text"), Some((200, "text".to_owned())));
        assert_eq!(writer.http_get("/missing"), Some((404, "missing".to_owned())));
        assert_eq!(
            writer.http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/admin"),
            Some((200, "admin".to_owned()))
        );
        assert_eq!(
            writer.control_get_as(ADMIN_USER, ADMIN_PASSWORD, "/availability/v1/status"),
            Some((200, "{\"consensus\":{\"leader\":\"dc-a\"}}".to_owned()))
        );
        assert_eq!(writer.download("/binary"), Some((200, vec![0, 159, 146, 150])));
        assert_eq!(
            writer
                .request(reqwest::Method::POST, "/request")
                .send()
                .expect("send request")
                .status()
                .as_u16(),
            201
        );
        assert_eq!(writer.http_get("/broken"), Some((200, String::new())));
        assert!(writer.download("/broken").is_none());
        assert!(!writer.log().is_empty());
        assert!(!writer.log_tail().is_empty());
        assert!(writer.diagnostics().contains("process: running"));
    });
}

#[test]
fn cluster_public_behavior() {
    with_fixture(|fixture| {
        let cluster = start_dc_cluster(fixture);
        fs::write(fixture.state(), "transfer:dc-b").expect("schedule leader transfer");
        assert_eq!(
            cluster
                .await_leader_change("dc-a", Duration::from_secs(1))
                .expect("observe transfer"),
            "dc-b"
        );
        fs::write(fixture.state(), "leader:dc-a").expect("restore leader");
        assert_eq!(
            cluster
                .await_leader_change("dc-b", Duration::from_secs(1))
                .expect("observe restored leader"),
            "dc-a"
        );
        let error = cluster
            .await_authority_transfer("dc-a", Duration::ZERO)
            .expect_err("reject unchanged leader");
        assert!(
            matches!(
                &error,
                HarnessError::NoTransfer { observed: Some(leader), .. } if leader == "dc-a"
            ),
            "unexpected transfer error: {error:?}"
        );

        for state in ["control-500", "control-invalid", "control-empty"] {
            fs::write(fixture.state(), state).expect("set control response");
            assert_eq!(cluster.leader().expect("read unsettled leader"), None);
            assert!(matches!(
                cluster.await_leader(Duration::ZERO),
                Err(HarnessError::NoLeader { .. })
            ));
        }
        fs::write(fixture.state(), "leader:dc-a").expect("restore control response");

        let report = cluster.failure_report();
        assert_eq!(report.nodes.len(), 2);
        assert!(report.render().contains("== node writer =="));
    });
}

#[test]
fn cluster_transfer_failure_preserves_last_observed_leader() {
    with_fixture(|fixture| {
        let cluster = start_dc_cluster(fixture);
        fs::write(fixture.state(), "leader-until-stream-error").expect("schedule topology stream error");

        let error = cluster
            .await_authority_transfer("dc-a", Duration::from_secs(90))
            .expect_err("reject unchanged leader");

        assert!(
            matches!(
                &error,
                HarnessError::NoTransfer { observed: Some(leader), .. } if leader == "dc-a"
            ),
            "unexpected transfer error: {error:?}"
        );
        assert_eq!(
            fs::read_to_string(fixture.state()).expect("read fixture state"),
            "control-500"
        );
    });
}

#[test]
fn node_process_lifecycle() {
    with_fixture(|fixture| {
        let mut cluster = start_dc_cluster(fixture);
        let writer = &mut cluster.nodes_mut()[0];
        let pid = writer.pid();
        writer.kill();
        assert!(!writer.is_running());
        assert!(!process_alive(pid));
        assert!(matches!(
            writer.await_log_signal(FAILURE_TIMEOUT, "event after exit"),
            Err(HarnessError::SignalClosed { .. })
        ));
        assert!(writer.diagnostics().contains("process: not running"));
        assert_eq!(
            (
                writer.status(),
                writer.readiness(),
                writer.topology(),
                writer.metrics(),
                writer.placements(),
                writer.http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/admin"),
                writer.control_get_as(ADMIN_USER, ADMIN_PASSWORD, "/availability/v1/status"),
                writer.download("/binary"),
            ),
            (None, None, None, None, None, None, None, None)
        );
        assert!(cluster.failure_report().nodes[0].process.starts_with("not running"));
        let writer = &mut cluster.nodes_mut()[0];
        writer.restart().expect("restart node");
        assert!(writer.is_ready());
    });
}

#[test]
fn topology_signal_reports_observable_outcomes() {
    with_fixture(|fixture| {
        let mut cluster = fixture
            .topology(Topology::single())
            .start()
            .expect("start signal fixture");
        cluster.nodes_mut()[0]
            .await_ready()
            .expect("an already-ready node remains ready");
        fs::write(fixture.state(), "status-broken").expect("break ready response");
        assert!(matches!(
            cluster.nodes_mut()[0].await_ready(),
            Err(HarnessError::NotReady { .. })
        ));
        fs::write(fixture.state(), "leader:dc-a").expect("restore ready response");
        let node = &cluster.nodes()[0];
        node.await_event("fixture event")
            .expect("receive a queued process event");
        node.await_event("fixture event")
            .expect("observe a persisted process event");
        assert!(matches!(
            node.await_log_signal(FAILURE_TIMEOUT, "missing fixture event"),
            Err(HarnessError::SignalTimeout { .. })
        ));
        assert_eq!(
            node.await_topology_signal(Duration::ZERO, |_| (Some("current"), "unused".to_owned()))
                .expect("accept current state"),
            "current",
        );
        let observations = Cell::new(0);
        assert_eq!(
            node.await_topology_event(|_| {
                observations.set(observations.get() + 1);
                (
                    (observations.get() == 2).then_some("event"),
                    format!("observation {}", observations.get()),
                )
            })
            .expect("accept state after a topology event"),
            "event",
        );
        assert_eq!(observations.get(), 2);

        assert!(matches!(
            node.await_topology_signal::<()>(Duration::from_secs(1), |_| (None, "closed".to_owned())),
            Err(HarnessError::SignalClosed { last, .. }) if last == "closed"
        ));
        fs::write(fixture.state(), "stream-503").expect("reject topology stream");
        assert!(matches!(
            node.await_topology_signal::<()>(Duration::from_secs(1), |_| (None, "status".to_owned())),
            Err(HarnessError::SignalRead { failure, .. }) if failure == "HTTP 503 Service Unavailable"
        ));
        fs::write(fixture.state(), "stream-broken").expect("break topology stream");
        assert!(matches!(
            node.await_topology_signal::<()>(Duration::from_secs(1), |_| (None, "broken".to_owned())),
            Err(HarnessError::SignalRead { .. })
        ));
        fs::write(fixture.state(), "stream-silent").expect("silence topology stream");
        assert!(matches!(
            node.await_topology_signal::<()>(FAILURE_TIMEOUT, |_| (None, "silent".to_owned())),
            Err(HarnessError::SignalTimeout { last, .. }) if last == "silent"
        ));

        cluster.nodes_mut()[0].kill();
        let node = &cluster.nodes()[0];
        assert!(matches!(
            node.await_topology_signal::<()>(FAILURE_TIMEOUT, |_| (None, "dead".to_owned())),
            Err(HarnessError::SignalRead { .. })
        ));
        assert!(matches!(
            cluster.await_topology_signal::<()>(FAILURE_TIMEOUT, |_| (None, "dead cluster".to_owned())),
            Err(HarnessError::SignalClosed { node, last }) if node == "cluster" && last == "dead cluster"
        ));
    });
}

#[test]
fn process_signal_failure_reaps_the_cluster() {
    with_fixture(|fixture| {
        let mut pids = Vec::new();
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let cluster = fixture
                .topology(Topology::dc("group-a", members()))
                .start()
                .expect("start signal fixture");
            pids.extend(cluster.nodes().iter().map(Node::pid));
            cluster.nodes()[0]
                .await_log_signal(FAILURE_TIMEOUT, "missing fixture event")
                .expect("missing event fails the test");
        }));
        assert!(failure.is_err());
        assert_eq!(pids.len(), 2);
        assert!(pids.into_iter().all(|pid| !process_alive(pid)));
    });
}

#[test]
fn process_limit_releases_capacity_with_the_child() {
    with_fixture(|fixture| {
        let limit = ProcessLimit::new(1);
        let first = fixture
            .harness()
            .with_process_limit(limit.clone())
            .spawn_with_config("first", "")
            .expect("start first node");
        let harness = fixture.harness().with_process_limit(limit);
        let (started, receiver) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let second = harness.spawn_with_config("second", "").expect("start second node");
                started.send(second).expect("report second node");
            });
            assert!(matches!(
                receiver.recv_timeout(FAILURE_TIMEOUT),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            drop(first);
            assert!(
                receiver
                    .recv_timeout(Duration::from_secs(1))
                    .expect("second node starts")
                    .is_ready()
            );
        });
    });
}

#[test]
#[should_panic(expected = "a process limit must allow at least one child")]
fn process_limit_rejects_zero_capacity() {
    let _ = ProcessLimit::new(0);
}

#[test]
fn process_explicit_port_collision_and_listener_ownership() {
    with_fixture(|fixture| {
        let occupied = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let port = occupied.local_addr().expect("reserved address").port();
        let collision = fixture
            .harness()
            .spawn_on_port("collision", port)
            .expect_err("port collision must fail");
        assert!(matches!(collision, HarnessError::ExitedEarly { .. }));

        let mut cluster = fixture
            .topology(Topology::dc("group-a", members()))
            .start()
            .expect("start inherited listeners");
        let node = &mut cluster.nodes_mut()[0];
        let public_port = node.port();
        let control_port = node
            .control_endpoint()
            .rsplit_once(':')
            .expect("control endpoint")
            .1
            .parse::<u16>()
            .expect("control port");
        node.kill();
        let public = TcpListener::bind(("127.0.0.1", public_port)).expect("parent released public listener");
        let control = TcpListener::bind(("127.0.0.1", control_port)).expect("parent released control listener");
        drop((public, control));
        node.restart().expect("restart with inherited listeners");
    });
}

#[test]
fn process_released_port_reports_readiness_and_timeout() {
    with_fixture(|fixture| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve ready port");
        let port = listener.local_addr().expect("ready port").port();
        drop(listener);
        let node = fixture
            .harness()
            .spawn_on_port("released", port)
            .expect("start released port");
        assert!(node.is_ready());
        drop(node);

        fs::write(fixture.serve_mode(), "hang").expect("hold released port unready");
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve unready port");
        let port = listener.local_addr().expect("unready port").port();
        drop(listener);
        assert!(matches!(
            fixture
                .harness()
                .with_ready_timeout(FAILURE_TIMEOUT)
                .spawn_on_port("unready", port),
            Err(HarnessError::NotReady { .. })
        ));
    });
}

#[test]
fn process_silent_start_reports_the_deadlock_guard() {
    with_fixture(|fixture| {
        fs::write(fixture.serve_mode(), "silent-hang").expect("suppress process events");
        let error = fixture
            .harness()
            .with_deadlock_guard(Duration::ZERO)
            .spawn_with_config("silent", "")
            .expect_err("silent process must hit the deadlock guard");

        assert!(matches!(
            error,
            HarnessError::NotReady { timeout, log, .. }
                if timeout.is_zero() && log.contains("process event missing")
        ));
    });
}

#[cfg(unix)]
#[test]
fn process_ready_reports_an_external_reap() {
    with_fixture(|fixture| {
        fs::write(fixture.serve_mode(), "hang").expect("hold process before readiness");
        let mut node = fixture
            .harness()
            .spawn_until_event("reaped", "", "fixture process started")
            .expect("observe process start");
        reap_process(node.pid(), Some(nix::sys::signal::Signal::SIGKILL));

        assert!(matches!(
            node.await_ready(),
            Err(HarnessError::Io(error)) if error.raw_os_error() == Some(nix::libc::ECHILD)
        ));
    });
}

#[test]
fn process_ready_reports_a_stopped_node() {
    with_fixture(|fixture| {
        let mut node = fixture.harness().spawn_with_config("stopped", "").expect("start node");
        node.kill();

        assert!(matches!(
            node.await_ready(),
            Err(HarnessError::NotReady { timeout, log, .. })
                if timeout.is_zero() && log.contains("process stopped")
        ));
    });
}

#[test]
fn process_accepts_the_startup_signal_as_its_first_event() {
    with_fixture(|fixture| {
        fs::write(fixture.serve_mode(), "direct-startup").expect("start with the readiness event");
        let node = fixture
            .harness()
            .spawn_with_config("direct", "")
            .expect("start from the readiness event");

        assert!(node.is_ready());
    });
}

#[test]
fn process_reports_failure_after_startup_signal() {
    with_fixture(|fixture| {
        fs::write(fixture.serve_mode(), "signal-only").expect("set startup mode");
        let node = fixture
            .harness()
            .spawn_until_event("signal-only", "", "fixture signal-only")
            .expect("observe signal-only startup");
        assert!(!node.is_ready());
    });
}

#[test]
fn process_event_startup_reports_executable_failure() {
    with_fixture(|fixture| {
        assert!(matches!(
            ProcessHarness::new(fixture.missing()).spawn_until_event("missing", "", "unreachable"),
            Err(HarnessError::Io(_))
        ));
    });
}

#[test]
fn process_raw_config_failures() {
    with_fixture(|fixture| {
        let error = fixture
            .harness()
            .spawn_with_config("invalid", "invalid = [")
            .expect_err("invalid config must fail startup");
        assert!(matches!(error, HarnessError::ExitedEarly { log, .. } if log.contains("invalid config")));
        let mut raw = fixture.harness().spawn_with_config("raw", "").expect("start raw node");
        assert_eq!(raw.identity(), "raw");
        raw.restart().expect("restart raw node");
        drop(raw);
        assert!(matches!(
            fixture
                .topology(Topology::single())
                .with_index_config("reject = true")
                .validate_config(),
            Err(HarnessError::Config(_))
        ));
    });
}

#[test]
fn process_prepares_member_data_before_start() {
    with_fixture(|fixture| {
        let prepared = Cell::new(false);
        let cluster = fixture
            .topology(Topology::single())
            .start_with_data(|member, path| {
                assert_eq!(member.node, "node-a");
                assert!(path.is_dir());
                prepared.set(true);
            })
            .expect("start prepared node");
        assert!(prepared.get());
        assert_eq!(cluster.nodes().len(), 1);
    });
}

#[test]
fn process_reports_status_body_and_executable_failures() {
    with_fixture(|fixture| {
        fs::write(fixture.state(), "status-broken").expect("truncate status body");
        assert!(matches!(
            fixture
                .harness()
                .with_ready_timeout(FAILURE_TIMEOUT)
                .spawn_with_config("broken-status", ""),
            Err(HarnessError::NotReady { .. })
        ));
        assert!(matches!(
            ProcessHarness::new(fixture.missing()).spawn_with_config("missing", ""),
            Err(HarnessError::Io(_))
        ));

        fs::write(fixture.state(), "leader:dc-a").expect("restore status body");
        let mut node = fixture
            .harness()
            .spawn_with_config("restart", "")
            .expect("start restart node");
        node.kill();
        fs::remove_file(fixture.peryx()).expect("remove process executable");
        assert!(matches!(node.restart(), Err(HarnessError::Io(_))));
    });
}

#[test]
fn process_reports_a_missing_path_executable() {
    let error = ProcessHarness::new(format!("peryx-missing-{}", std::process::id()))
        .spawn_with_config("missing", "")
        .expect_err("a missing PATH executable must fail");

    assert!(matches!(error, HarnessError::Io(error) if error.kind() == std::io::ErrorKind::NotFound));
}

#[test]
fn process_bootstrap_and_claim_failures() {
    with_fixture(|fixture| {
        fs::write(fixture.bootstrap_mode(), "fail").expect("reject bootstrap");
        assert!(matches!(
            fixture.topology(Topology::single()).with_admin().start(),
            Err(HarnessError::Config(_))
        ));

        fs::write(fixture.claim_mode(), "fail").expect("reject claim");
        assert!(matches!(
            fixture.topology(Topology::dc("group-a", members())).start(),
            Err(HarnessError::Config(_))
        ));
    });
}

#[test]
fn fixture_process_rejects_a_malformed_serve_command() {
    with_fixture(|fixture| {
        let output = Command::new(fixture.peryx())
            .args(["serve", "--config"])
            .arg(fixture.state())
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run malformed fixture request");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("missing argument --port"));
    });
}

#[test]
fn process_alive_rejects_invalid_pid() {
    assert!(!process_alive(u32::MAX));
}

#[test]
fn toxiproxy_public_behavior() {
    with_fixture(|fixture| {
        let mut toxiproxy = fixture.start_toxiproxy();
        assert!(toxiproxy.control_is_up());
        let proxy = toxiproxy.proxy("127.0.0.1:9000").expect("create proxy");
        assert!(proxy.endpoint().starts_with("127.0.0.1:"));
        toxiproxy.proxy("127.0.0.1:9001").expect("create second proxy");
        proxy.partition().expect("partition proxy");
        proxy.heal().expect("heal proxy");
        proxy.pause(Duration::MAX).expect("pause proxy");
        proxy.resume().expect("resume proxy");

        fs::write(fixture.toxi_state(), "error").expect("reject requests");
        assert!(matches!(
            toxiproxy.proxy("127.0.0.1:9000"),
            Err(HarnessError::Toxiproxy(_))
        ));
        assert!(matches!(proxy.partition(), Err(HarnessError::Toxiproxy(_))));
        assert!(matches!(
            proxy.pause(Duration::from_millis(1)),
            Err(HarnessError::Toxiproxy(_))
        ));
        assert!(matches!(proxy.resume(), Err(HarnessError::Toxiproxy(_))));
        fs::write(fixture.toxi_state(), "ok").expect("accept requests");

        toxiproxy.kill();
        assert!(!toxiproxy.control_is_up());
        assert!(matches!(
            toxiproxy.proxy("127.0.0.1:9000"),
            Err(HarnessError::Toxiproxy(_))
        ));
        assert!(matches!(proxy.heal(), Err(HarnessError::Toxiproxy(_))));
        assert!(matches!(
            proxy.pause(Duration::from_millis(1)),
            Err(HarnessError::Toxiproxy(_))
        ));
        assert!(matches!(proxy.resume(), Err(HarnessError::Toxiproxy(_))));

        assert!(serve_once(200, reachable_through));
        assert!(!serve_once(503, reachable_through));
        assert!(serve_once(200, request_until_eof));
        assert!(!reachable_through("127.0.0.1:1"));
    });
}

#[test]
fn toxiproxy_startup_reports_process_exit() {
    with_fixture(|fixture| {
        fs::write(fixture.toxi_mode(), "exit").expect("exit during startup");
        let error = fixture
            .start_toxiproxy_with_timeout(TOXIPROXY_FAILURE_TIMEOUT)
            .err()
            .expect("startup must fail");
        assert!(error.to_string().contains("process status:"), "{error}");
        assert!(!process_alive(fixture.toxiproxy_pid()));
    });
}

#[cfg(unix)]
#[test]
fn toxiproxy_startup_reports_an_external_reap() {
    with_fixture(|fixture| {
        let (startup, receiver, output_descriptors) = externally_reapable_toxiproxy_start(fixture);
        reap_process(fixture.toxiproxy_pid(), None);
        drop(output_descriptors);

        let error = receiver
            .recv_timeout(TOXIPROXY_FAILURE_TIMEOUT)
            .expect("receive bounded startup result")
            .err()
            .expect("an externally reaped process must fail startup");
        startup.join().expect("join startup thread");

        assert_eq!(
            error.to_string(),
            format!(
                "toxiproxy: read control process event: {}",
                std::io::Error::from_raw_os_error(nix::libc::ECHILD)
            )
        );
    });
}

#[test]
fn toxiproxy_startup_waits_for_the_version_endpoint() {
    with_fixture(|fixture| {
        let (startup, receiver, mut release) = gated_toxiproxy_start(fixture);
        assert!(matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let mut published_port = [0; 2];
        release.read_exact(&mut published_port).expect("read control port");
        assert_ne!(u16::from_be_bytes(published_port), 0);
        release
            .shutdown(std::net::Shutdown::Write)
            .expect("release control listener");
        let mut toxiproxy = receiver
            .recv_timeout(TOXIPROXY_FAILURE_TIMEOUT)
            .expect("receive bounded startup result")
            .expect("start after version responds");
        startup.join().expect("join startup thread");
        assert!(toxiproxy.control_is_up());
        toxiproxy.kill();
    });
}

#[test]
fn toxiproxy_startup_waits_through_a_not_found_version_route() {
    with_fixture(|fixture| {
        fs::write(fixture.toxi_mode(), "event-ready").expect("emit a process event before startup");
        fs::write(fixture.toxi_state(), "startup-not-found").expect("delay version route");
        let mut toxiproxy = fixture
            .start_toxiproxy_with_timeout(TOXIPROXY_FAILURE_TIMEOUT)
            .expect("start after the version route is mounted");
        assert!(toxiproxy.control_is_up());
        toxiproxy.kill();
    });
}

#[test]
fn toxiproxy_startup_reports_exit_after_the_signal() {
    with_fixture(|fixture| {
        fs::write(fixture.toxi_mode(), "signal-exit").expect("exit after startup signal");
        let error = fixture
            .start_toxiproxy_with_timeout(TOXIPROXY_FAILURE_TIMEOUT)
            .err()
            .expect("startup must fail");
        assert!(
            error.to_string().contains("exited before accepting requests"),
            "{error}"
        );
        assert!(!process_alive(fixture.toxiproxy_pid()));
    });
}

#[test]
fn toxiproxy_startup_reports_control_failure() {
    with_fixture(|fixture| {
        fs::write(fixture.toxi_state(), "startup-error").expect("reject startup check");
        let error = fixture
            .start_toxiproxy_with_timeout(TOXIPROXY_FAILURE_TIMEOUT)
            .err()
            .expect("control failure must fail startup");
        assert!(error.to_string().contains("500 Internal Server Error"), "{error}");
        assert!(!process_alive(fixture.toxiproxy_pid()));
    });
}

#[test]
fn toxiproxy_startup_reports_timeout() {
    with_fixture(|fixture| {
        let error = fixture
            .start_toxiproxy_with_timeout(Duration::ZERO)
            .err()
            .expect("startup must time out");
        assert_eq!(
            error.to_string(),
            "toxiproxy: control API did not start within 0ns; process was not spawned"
        );
        assert!(!fixture.path().join("toxi-pid").exists());
    });
}

#[test]
#[cfg(unix)]
fn toxiproxy_startup_event_guard_reports_a_silent_process() {
    with_fixture(|fixture| {
        fs::write(fixture.toxiproxy(), "#!/bin/sh\nread _request\n").expect("install silent process fixture");
        fs::set_permissions(fixture.toxiproxy(), fs::Permissions::from_mode(0o700))
            .expect("make silent process fixture executable");

        let error = Toxiproxy::start_with(fixture.toxiproxy(), TOXIPROXY_FAILURE_TIMEOUT, Duration::ZERO)
            .err()
            .expect("process event must time out");

        assert_eq!(
            error.to_string(),
            "toxiproxy: control process did not emit an event within 0ns; process status: None"
        );
    });
}

#[test]
fn toxiproxy_startup_signal_timeout_reaps_the_process() {
    with_fixture(|fixture| {
        let (startup, receiver, startup_signal) = silent_toxiproxy_start(fixture);
        drop(startup_signal);
        let error = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("receive bounded startup result")
            .err()
            .expect("startup signal must time out");
        startup.join().expect("join startup thread");
        assert_eq!(
            error.to_string(),
            "toxiproxy: control process did not emit its startup signal within 2s; process status: None"
        );
        assert!(!process_alive(fixture.toxiproxy_pid()));
    });
}

#[test]
#[should_panic(expected = "fixture gate not received within 0ns")]
fn toxiproxy_gate_timeout_is_bounded() {
    let gate = TcpListener::bind("127.0.0.1:0").expect("bind timeout gate");
    drop(accept_within(&gate, Duration::ZERO, "fixture gate"));
}

#[test]
fn toxiproxy_readiness_timeout_reaps_the_process() {
    with_fixture(|fixture| {
        let (startup, receiver, readiness_gate) = gated_toxiproxy_start(fixture);
        let error = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("receive bounded startup result")
            .err()
            .expect("readiness must time out");
        startup.join().expect("join startup thread");
        assert!(
            error.to_string().contains("did not accept requests within 2s"),
            "{error}"
        );
        drop(readiness_gate);
        assert!(!process_alive(fixture.toxiproxy_pid()));
    });
}

#[test]
fn toxiproxy_rejects_a_foreign_control_api() {
    with_fixture(|fixture| {
        let (startup, receiver, mut child) = toxiproxy_start_at_gate(fixture, "gate");
        let mut port = [0; 2];
        child.read_exact(&mut port).expect("read selected control port");
        let foreign = TcpListener::bind(("127.0.0.1", u16::from_be_bytes(port))).expect("bind foreign control API");
        let control = foreign.local_addr().expect("foreign control address");
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            loop {
                let (mut stream, _) = foreign.accept().expect("accept control request");
                let request = read_request(&mut stream);
                let shutdown = request.starts_with("GET /shutdown ");
                let status = if request.starts_with("GET /version ") { 200 } else { 404 };
                write_response(&mut stream, status, b"{}", 2);
                requests.push(request);
                if shutdown {
                    return requests;
                }
            }
        });

        let result = receiver
            .recv_timeout(FIXTURE_DEADLOCK_GUARD)
            .expect("receive bounded startup result");
        let error = result.as_ref().err().map(ToString::to_string);
        drop(child);
        drop(result);
        let mut shutdown = TcpStream::connect(control).expect("connect foreign shutdown");
        shutdown
            .write_all(b"GET /shutdown HTTP/1.1\r\n\r\n")
            .expect("request foreign shutdown");
        shutdown.read_to_end(&mut Vec::new()).expect("read foreign shutdown");
        let requests = server.join().expect("join foreign control API");
        startup.join().expect("join toxiproxy startup");

        assert_eq!(
            error.as_deref(),
            Some("toxiproxy: control API does not belong to the spawned process")
        );
        assert!(
            requests
                .iter()
                .any(|request| request.starts_with("GET /proxies/peryx-control-")),
            "the harness did not verify control ownership: {requests:?}",
        );
    });
}

#[test]
fn toxiproxy_fixture_reports_rejected_shutdown() {
    with_fixture(|fixture| {
        let gate = TcpListener::bind("127.0.0.1:0").expect("bind shutdown gate");
        fs::write(
            fixture.toxi_mode(),
            format!(
                "shutdown-gate:{}",
                gate.local_addr().expect("shutdown gate address").port()
            ),
        )
        .expect("reject graceful shutdown");
        let control = TcpListener::bind("127.0.0.1:0").expect("reserve control port");
        let port = control.local_addr().expect("control address").port();
        drop(control);
        let mut process = Command::new(fixture.toxiproxy())
            .args(["-host", "127.0.0.1", "-port"])
            .arg(port.to_string())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start toxiproxy fixture");
        let mut startup = String::new();
        BufReader::new(process.stdout.take().expect("fixture stdout"))
            .read_line(&mut startup)
            .expect("read startup event");
        assert!(startup.contains("Starting Toxiproxy HTTP server"), "{startup}");

        let mut request = TcpStream::connect(("127.0.0.1", port)).expect("connect control API");
        request
            .write_all(b"POST /shutdown HTTP/1.1\r\nHost: fixture\r\nContent-Length: 0\r\n\r\n")
            .expect("request fixture shutdown");
        let mut child = accept_within(&gate, FIXTURE_DEADLOCK_GUARD, "shutdown request event");
        child.read_exact(&mut [0]).expect("identify shutdown request");
        let response = read_request(&mut request);
        child.write_all(&[1]).expect("release fixture shutdown");

        assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
        assert!(process.wait().expect("reap toxiproxy fixture").success());
    });
}

#[test]
fn toxiproxy_kills_its_child_after_shutdown_is_rejected() {
    with_fixture(|fixture| {
        let gate = TcpListener::bind("127.0.0.1:0").expect("bind shutdown gate");
        fs::write(
            fixture.toxi_mode(),
            format!(
                "shutdown-gate:{}",
                gate.local_addr().expect("shutdown gate address").port()
            ),
        )
        .expect("reject graceful shutdown");
        let mut toxiproxy = fixture.start_toxiproxy();
        let pid = fixture.toxiproxy_pid();
        let (sender, receiver) = mpsc::sync_channel(1);

        thread::scope(|scope| {
            let stop = scope.spawn(|| {
                toxiproxy.kill();
                sender.send(()).expect("publish shutdown completion");
            });
            let mut child = accept_within(&gate, FIXTURE_DEADLOCK_GUARD, "shutdown request event");
            child.read_exact(&mut [0]).expect("identify shutdown request");
            receiver
                .recv_timeout(FIXTURE_DEADLOCK_GUARD)
                .expect("receive shutdown completion");
            stop.join().expect("join toxiproxy shutdown");
            assert!(
                !process_alive(pid),
                "fixture child remained alive after rejected shutdown"
            );
        });
    });
}

#[test]
fn proxied_topology_public_behavior() {
    with_fixture(|fixture| {
        let mut toxiproxy = fixture.start_toxiproxy();

        let mut proxied = fixture
            .topology(Topology::dc("group-a", members()))
            .start_proxied(&mut toxiproxy, false)
            .expect("start proxied cluster");
        assert_eq!(proxied.cluster().nodes().len(), 2);
        assert_eq!(proxied.cluster_mut().nodes_mut().len(), 2);
        assert!(proxied.proxy("replica").is_some());
        assert!(proxied.proxy("writer").is_none());
        let healthy = fixture
            .topology(Topology::dc("group-a", members()))
            .start_proxied(&mut toxiproxy, true)
            .expect("start healthy proxied cluster");
        assert_eq!(healthy.cluster().nodes().len(), 2);
    });
}

#[test]
fn toxiproxy_uses_its_bound_proxy_address() {
    with_fixture(|fixture| {
        let mut toxiproxy = fixture.start_toxiproxy();
        let proxy = toxiproxy.proxy("127.0.0.1:1").expect("create proxy");

        assert_eq!(proxy.endpoint(), "127.0.0.1:23456");
    });
}

#[test]
fn toxiproxy_rejects_invalid_proxy_responses() {
    with_fixture(|fixture| {
        let mut toxiproxy = fixture.start_toxiproxy();
        let errors = ["proxy-malformed", "proxy-missing-listen"].map(|state| {
            fs::write(fixture.toxi_state(), state).expect("configure proxy response");
            toxiproxy
                .proxy("127.0.0.1:1")
                .err()
                .expect("invalid proxy response must fail")
                .to_string()
        });

        assert!(errors[0].starts_with("toxiproxy: decode POST /proxies:"));
        assert_eq!(errors[1], "toxiproxy: POST /proxies omitted its bound address");
    });
}

#[test]
fn proxied_topology_reports_member_startup_failure() {
    with_fixture(|fixture| {
        fs::write(fixture.claim_mode(), "fail").expect("reject replica claim");
        let mut toxiproxy = fixture.start_toxiproxy();
        assert!(matches!(
            fixture
                .topology(Topology::dc("group-a", members()))
                .start_proxied(&mut toxiproxy, true),
            Err(HarnessError::Config(_))
        ));
    });
}

fn members() -> Vec<MemberSpec> {
    vec![
        MemberSpec::new("writer", "dc-a", Role::Writer),
        MemberSpec::new("replica", "dc-a", Role::Replica),
    ]
}

fn start_dc_cluster(fixture: &FixtureEnvironment) -> Cluster {
    fixture
        .topology(Topology::dc("group-a", members()))
        .with_admin()
        .start()
        .expect("start dc cluster")
}

struct FixtureEnvironment {
    dir: TempDir,
}

impl FixtureEnvironment {
    fn new() -> Self {
        let dir = TempDir::new().expect("create fixture directory");
        let fixture = Self { dir };
        fs::copy(cargo_binary("peryx-test-fixture"), fixture.peryx()).expect("install process fixture");
        fs::copy(fixture.peryx(), fixture.toxiproxy()).expect("install toxiproxy fixture");
        fs::write(fixture.state(), "leader:dc-a").expect("write state");
        fs::write(fixture.toxi_state(), "ok").expect("write toxiproxy state");
        fs::write(fixture.bootstrap_mode(), "ok").expect("write bootstrap mode");
        fs::write(fixture.claim_mode(), "ok").expect("write claim mode");
        fs::write(fixture.serve_mode(), "ready").expect("write serve mode");
        fs::write(fixture.toxi_mode(), "ready").expect("write toxiproxy mode");
        fixture
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn peryx(&self) -> PathBuf {
        self.path().join(format!("peryx{}", std::env::consts::EXE_SUFFIX))
    }

    fn toxiproxy(&self) -> PathBuf {
        self.path()
            .join(format!("toxiproxy-server{}", std::env::consts::EXE_SUFFIX))
    }

    fn missing(&self) -> PathBuf {
        self.path().join("missing")
    }

    fn state(&self) -> PathBuf {
        self.path().join("state")
    }

    fn toxi_state(&self) -> PathBuf {
        self.path().join("toxi-state")
    }

    fn claim_mode(&self) -> PathBuf {
        self.path().join("claim-mode")
    }

    fn bootstrap_mode(&self) -> PathBuf {
        self.path().join("bootstrap-mode")
    }

    fn serve_mode(&self) -> PathBuf {
        self.path().join("serve-mode")
    }

    fn toxi_mode(&self) -> PathBuf {
        self.path().join("toxi-mode")
    }

    fn toxiproxy_pid(&self) -> u32 {
        fs::read_to_string(self.path().join("toxi-pid"))
            .expect("read toxiproxy pid")
            .parse()
            .expect("parse toxiproxy pid")
    }

    fn harness(&self) -> ProcessHarness {
        ProcessHarness::new(self.peryx()).with_shutdown_path("/__fixture/shutdown")
    }

    fn topology(&self, topology: Topology) -> Topology {
        topology.with_process_harness(self.harness())
    }

    fn start_toxiproxy(&self) -> Toxiproxy {
        self.start_toxiproxy_with_timeout(Duration::from_secs(10))
            .expect("start toxiproxy")
    }

    fn start_toxiproxy_with_timeout(&self, timeout: Duration) -> Result<Toxiproxy, HarnessError> {
        Toxiproxy::start_with(self.toxiproxy(), timeout, FIXTURE_DEADLOCK_GUARD)
    }
}

fn with_fixture(test: impl FnOnce(&FixtureEnvironment)) {
    let fixture = FixtureEnvironment::new();
    test(&fixture);
}

fn gated_toxiproxy_start(
    fixture: &FixtureEnvironment,
) -> (
    thread::JoinHandle<()>,
    mpsc::Receiver<Result<Toxiproxy, HarnessError>>,
    TcpStream,
) {
    toxiproxy_start_at_gate(fixture, "gate")
}

fn silent_toxiproxy_start(
    fixture: &FixtureEnvironment,
) -> (
    thread::JoinHandle<()>,
    mpsc::Receiver<Result<Toxiproxy, HarnessError>>,
    TcpStream,
) {
    toxiproxy_start_at_gate(fixture, "event-silent-gate")
}

#[cfg(unix)]
fn externally_reapable_toxiproxy_start(
    fixture: &FixtureEnvironment,
) -> (
    thread::JoinHandle<()>,
    mpsc::Receiver<Result<Toxiproxy, HarnessError>>,
    [OwnedFd; 2],
) {
    let path = fixture.path().join("external-reap.sock");
    let listener = UnixListener::bind(&path).expect("bind output descriptor socket");
    fs::write(fixture.toxi_mode(), format!("external-reap:{}", path.display())).expect("configure external reap");
    let (startup, receiver) = toxiproxy_start(fixture);
    let stream = accept_unix_within(&listener, FIXTURE_DEADLOCK_GUARD, "output descriptor transfer");
    (startup, receiver, receive_output_descriptors(&stream))
}

fn toxiproxy_start_at_gate(
    fixture: &FixtureEnvironment,
    mode: &str,
) -> (
    thread::JoinHandle<()>,
    mpsc::Receiver<Result<Toxiproxy, HarnessError>>,
    TcpStream,
) {
    let gate = TcpListener::bind("127.0.0.1:0").expect("bind readiness gate");
    fs::write(
        fixture.toxi_mode(),
        format!("{mode}:{}", gate.local_addr().expect("readiness gate address").port()),
    )
    .expect("configure readiness gate");
    let (startup, receiver) = toxiproxy_start(fixture);
    let connection = accept_within(&gate, FIXTURE_DEADLOCK_GUARD, "toxiproxy startup gate");
    (startup, receiver, connection)
}

fn toxiproxy_start(
    fixture: &FixtureEnvironment,
) -> (thread::JoinHandle<()>, mpsc::Receiver<Result<Toxiproxy, HarnessError>>) {
    let binary = fixture.toxiproxy();
    let (sender, receiver) = mpsc::sync_channel(1);
    let startup = thread::spawn(move || {
        sender
            .send(Toxiproxy::start_with(
                binary,
                TOXIPROXY_FAILURE_TIMEOUT,
                FIXTURE_DEADLOCK_GUARD,
            ))
            .expect("return startup result");
    });
    (startup, receiver)
}

fn accept_within(listener: &TcpListener, timeout: Duration, event: &str) -> TcpStream {
    let address = listener.local_addr().expect("gate address");
    let (cancel_timeout, cancellation) = mpsc::channel();
    let watchdog = thread::spawn(move || {
        let timed_out = matches!(cancellation.recv_timeout(timeout), Err(mpsc::RecvTimeoutError::Timeout));
        if timed_out {
            TcpStream::connect(address).expect("unblock timed-out gate accept");
        }
        timed_out
    });
    let connection = listener.accept().expect("accept gate connection").0;
    let _ = cancel_timeout.send(());
    if watchdog.join().expect("join gate watchdog") {
        drop(connection);
        panic!("{event} not received within {timeout:?}");
    }
    connection
}

#[cfg(unix)]
fn accept_unix_within(listener: &UnixListener, timeout: Duration, event: &str) -> UnixStream {
    let mut descriptors = [nix::poll::PollFd::new(listener.as_fd(), nix::poll::PollFlags::POLLIN)];
    assert_eq!(
        nix::poll::poll(
            &mut descriptors,
            nix::poll::PollTimeout::try_from(timeout).expect("deadlock guard fits poll timeout"),
        )
        .expect("poll Unix listener"),
        1,
        "{event} not received within {timeout:?}",
    );
    listener.accept().expect("accept Unix connection").0
}

#[cfg(unix)]
fn receive_output_descriptors(stream: &UnixStream) -> [OwnedFd; 2] {
    use unix_ancillary::UnixStreamExt as _;

    let message = stream.recv_fds_exact::<2>().expect("receive output descriptors");
    assert_eq!(message.data, [1]);
    message.fds.try_into().expect("stdout and stderr descriptors")
}

#[cfg(unix)]
fn reap_process(pid: u32, signal: Option<nix::sys::signal::Signal>) {
    let process = nix::unistd::Pid::from_raw(i32::try_from(pid).expect("pid fits an i32"));
    if let Some(signal) = signal {
        nix::sys::signal::kill(process, signal).expect("kill fixture process");
    }
    nix::sys::wait::waitpid(process, None).expect("reap fixture process outside its owner");
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    loop {
        let read = reader.read_until(b'\n', &mut bytes).expect("read request header");
        if read == 0 || bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_len = bytes.len();
    bytes.resize(header_len + request_body_len(&bytes), 0);
    reader.read_exact(&mut bytes[header_len..]).expect("read request body");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn request_body_len(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .map_or(0, |value| value.parse::<usize>().expect("content length"))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8], declared: usize) {
    write!(
        stream,
        "HTTP/1.1 {status} test\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
    )
    .expect("write response head");
    stream.write_all(body).expect("write response body");
}

fn serve_once<T>(status: u16, request: impl FnOnce(&str) -> T) -> T {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
    let endpoint = listener.local_addr().expect("test endpoint").to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept test request");
        let _ = read_request(&mut stream);
        write_response(&mut stream, status, b"ok", 2);
    });
    let result = request(&endpoint);
    server.join().expect("join test server");
    result
}

fn request_until_eof(endpoint: &str) -> bool {
    let mut stream = TcpStream::connect(endpoint).expect("connect test endpoint");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("finish empty request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response.starts_with("HTTP/1.1 200")
}
