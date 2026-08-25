use std::path::{Path, PathBuf};
use std::sync::Mutex;

use peryx_test_support::{HarnessError, ProcessHarness, Topology, Toxiproxy, spawn_on_port, spawn_with_config};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn process_fixture_rejects_missing_arguments() {
    let output = std::process::Command::new(peryx_test_support::cargo_binary("peryx-test-fixture"))
        .output()
        .expect("run process fixture");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected serve command"));
}

#[test]
fn process_harness_uses_the_configured_temporary_root() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().expect("create configured test root");
    let _lock = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    temp_env::with_var("PERYX_TEST_TMPDIR", Some(root.path()), || {
        let node = ProcessHarness::new(fixture.peryx())
            .spawn_with_config("configured-root", "")
            .expect("start fixture node");
        assert_eq!(std::fs::read_dir(root.path()).expect("read configured root").count(), 1);
        drop(node);
        assert_eq!(std::fs::read_dir(root.path()).expect("read configured root").count(), 0);
    });
}

#[test]
fn process_harness_uses_the_default_temporary_root() {
    let fixture = Fixture::new();
    let _lock = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    temp_env::with_var("PERYX_TEST_TMPDIR", None::<&Path>, || {
        let node = ProcessHarness::new(fixture.peryx())
            .spawn_with_config("default-root", "")
            .expect("start fixture node");
        assert_eq!(node.identity(), "default-root");
    });
}

#[test]
fn process_harness_resolves_the_configured_binary() {
    let fixture = Fixture::new();
    let _lock = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    temp_env::with_var("PERYX_BIN", Some(fixture.missing()), || {
        assert!(matches!(Topology::single().validate_config(), Err(HarnessError::Io(_))));
    });
    temp_env::with_var("PERYX_BIN", Some(fixture.peryx()), || {
        let raw = spawn_with_config("environment-raw", "").expect("start default raw node");
        assert_eq!(raw.identity(), "environment-raw");
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        assert!(matches!(
            spawn_on_port(
                "environment-collision",
                occupied.local_addr().expect("reserved address").port()
            ),
            Err(HarnessError::ExitedEarly { .. })
        ));
    });
    let path = std::env::join_paths(
        std::iter::once(fixture.path().to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    )
    .expect("fixture PATH");
    temp_env::with_var("PATH", Some(path), || {
        let node = ProcessHarness::new(fixture.peryx().file_name().expect("fixture executable"))
            .with_shutdown_path("/__fixture/shutdown")
            .spawn_with_config("path-node", "")
            .expect("resolve peryx from PATH");
        assert_eq!(node.identity(), "path-node");
    });
}

#[test]
fn toxiproxy_resolves_the_configured_binary() {
    let fixture = Fixture::new();
    let empty = tempfile::tempdir().expect("create empty path");
    let _lock = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    temp_env::with_var("PATH", Some(empty.path()), || {
        assert!(matches!(Toxiproxy::start(), Err(HarnessError::Toxiproxy(_))));
    });
    temp_env::with_var("PATH", Some(fixture.path()), || {
        let mut toxiproxy = Toxiproxy::start().expect("resolve toxiproxy from PATH");
        assert!(toxiproxy.control_is_up());
        toxiproxy.kill();
    });
}

struct Fixture {
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create fixture directory");
        let fixture = Self { directory };
        std::fs::copy(peryx_test_support::cargo_binary("peryx-test-fixture"), fixture.peryx())
            .expect("install process fixture");
        std::fs::copy(fixture.peryx(), fixture.toxiproxy()).expect("install toxiproxy fixture");
        std::fs::write(fixture.path().join("state"), "leader:dc-a").expect("write fixture state");
        std::fs::write(fixture.path().join("serve-mode"), "ready").expect("write fixture mode");
        std::fs::write(fixture.path().join("toxi-state"), "ok").expect("write toxiproxy state");
        std::fs::write(fixture.path().join("toxi-mode"), "ready").expect("write toxiproxy mode");
        fixture
    }

    fn path(&self) -> &Path {
        self.directory.path()
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
}
