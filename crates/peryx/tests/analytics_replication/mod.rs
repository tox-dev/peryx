//! Multiprocess proof that a producer's sealed daily analytics replicate to a replica over the wire,
//! and that a restarted replica resumes from its durable cursor without double-counting.
//!
//! Unlike the shared harness this spawns its own producer/replica pair, because the proof needs to seed
//! a sealed past-day aggregate directly into the producer's store before it boots - analytics are dated
//! off the real clock, so a live download can only land on today, never on a day already sealed. The
//! seed writes the same durable daily snapshot a live download would have persisted, using the
//! `test-util` encoder, and the producer restores and re-exports it as a sealed-day batch on startup.
//!
//! The convergence signal is the on-node `GET /+analytics/completeness` read surface: it re-reads the
//! replica's persisted analytics apply state and reports the converged totals plus each producer's
//! accepted `(epoch, sealed day)` frontier. The proof asserts through that endpoint the way an operator
//! would, never linking a peryx crate to reach into the running node's state.

use std::fmt::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use peryx_events::metrics::{DailyUsage, encode_daily_snapshot};
use peryx_storage::meta::MetaStore;
use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_peryx");
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const SECONDS_PER_DAY: i64 = 86_400;

const TOKEN: &str = "analytics-replication-token";
const GROUP: &str = "analytics";
const PRODUCER: &str = "producer";
const REPLICA: &str = "replica";
const ADMIN_USER: &str = "analytics-admin";
const ADMIN_PASSWORD: &str = "analytics-admin-secret";

const SEED_DOWNLOADS: u64 = 9;
const SEED_BYTES: u64 = 65_536;

#[test]
fn test_analytics_batches_replicate_and_survive_a_replica_restart() {
    let ports = Ports::draw();
    let sealed_day = utc_today() - 1;

    let mut producer = Node::start_producer(&ports, sealed_day);
    let mut replica = Node::start_replica(&ports);

    // The replica pulls the producer's sealed day over the real wire and folds it into its durable apply
    // state; its completeness surface converges to the producer's totals and records the accepted cursor.
    let converged = replica.await_convergence();
    assert_eq!(converged.downloads, SEED_DOWNLOADS, "converged download total");
    assert_eq!(converged.bytes, SEED_BYTES, "converged byte total");
    assert_eq!(
        converged.accepted_day,
        Some(sealed_day),
        "the replica records the producer's sealed-day cursor",
    );
    assert_eq!(
        converged.accepted_epoch,
        Some(1),
        "the producer's first analytics epoch"
    );

    // Cut the producer off before restarting the replica: with its upstream gone the replica cannot pull
    // anything, so any total it reports after coming back can only have come from its own durable store.
    producer.kill();
    replica.restart();

    let resumed = replica.await_totals();
    assert_eq!(
        resumed.downloads, SEED_DOWNLOADS,
        "the replica resumes its converged total from the durable cursor with its upstream offline",
    );
    assert_eq!(
        resumed.bytes, SEED_BYTES,
        "the resumed byte total is not double-counted"
    );
    assert_eq!(
        resumed.accepted_day,
        Some(sealed_day),
        "the durable cursor survives the restart",
    );
    // With the producer down the durable state is the only possible source, so the total is stable: a
    // lost cursor would read as zero here, a re-applied batch as twice the seed. It is exactly the seed.
    assert_eq!(
        replica.await_totals().downloads,
        SEED_DOWNLOADS,
        "the resumed total stays put"
    );
}

/// One converged reading of the replica's completeness surface: the accepted totals and the producer's
/// accepted `(epoch, sealed day)` frontier.
#[derive(Debug, Clone, Copy)]
struct Completeness {
    downloads: u64,
    bytes: u64,
    accepted_epoch: Option<u64>,
    accepted_day: Option<i64>,
}

/// The four loopback ports the pair binds, drawn once so the shared roster names the same addresses every
/// node reads.
struct Ports {
    producer_public: u16,
    producer_control: u16,
    replica_public: u16,
    replica_control: u16,
}

impl Ports {
    fn draw() -> Self {
        Self {
            producer_public: free_port(),
            producer_control: free_port(),
            replica_public: free_port(),
            replica_control: free_port(),
        }
    }
}

/// One running `peryx serve` process and the surface the proof drives it through, owning its data
/// directory so a restart reuses the same durable store and the drop tears the process group down.
struct Node {
    child: Child,
    data: TempDir,
    config: PathBuf,
    port: u16,
    http: reqwest::blocking::Client,
}

impl Node {
    fn start_producer(ports: &Ports, sealed_day: i64) -> Self {
        let data = TempDir::new().expect("producer data dir");
        let config = data.path().join("peryx.toml");
        std::fs::write(&config, producer_config(ports)).expect("write producer config");
        seed_sealed_day(data.path(), sealed_day);
        Self::launch_ready(data, config, ports.producer_public)
    }

    fn start_replica(ports: &Ports) -> Self {
        let data = TempDir::new().expect("replica data dir");
        let config = data.path().join("peryx.toml");
        std::fs::write(&config, replica_config(ports)).expect("write replica config");
        // A replica starts read-only and only verifies the writer identity, so its store must already
        // hold it, and it authenticates the operator that reads completeness - both seeded offline.
        run_offline(&config, data.path(), &["writer", "claim"]);
        bootstrap_admin(&config, data.path());
        Self::launch_ready(data, config, ports.replica_public)
    }

    fn launch_ready(data: TempDir, config: PathBuf, port: u16) -> Self {
        let mut node = Self {
            child: launch(&config, data.path(), port),
            data,
            config,
            port,
            http: reqwest::blocking::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("build http client"),
        };
        node.await_ready();
        node
    }

    fn await_ready(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("child status") {
                panic!("node exited during startup with {status}\n{}", self.log_tail());
            }
            if self.is_ready() {
                return;
            }
            std::thread::sleep(POLL);
        }
        panic!("node never became ready within {READY_TIMEOUT:?}\n{}", self.log_tail());
    }

    fn is_ready(&self) -> bool {
        self.http_get_as("/+status")
            .is_some_and(|(code, body)| code == 200 && body.contains("\"version\""))
    }

    /// Poll the operator completeness surface until the replica has folded the producer's full sealed day
    /// - both totals and the accepted cursor - or fail with the log tail after a generous deadline.
    fn await_convergence(&self) -> Completeness {
        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        loop {
            if let Some(view) = self.completeness()
                && view.downloads == SEED_DOWNLOADS
                && view.bytes == SEED_BYTES
                && view.accepted_day.is_some()
            {
                return view;
            }
            assert!(
                Instant::now() < deadline,
                "the replica never converged to the producer's totals\n{}",
                self.log_tail(),
            );
            std::thread::sleep(POLL);
        }
    }

    /// Poll until the completeness surface answers at all, returning whatever totals it reports. Used
    /// after a restart, where the durable state is present the moment the node serves.
    fn await_totals(&self) -> Completeness {
        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        loop {
            if let Some(view) = self.completeness() {
                return view;
            }
            assert!(
                Instant::now() < deadline,
                "the replica never answered completeness\n{}",
                self.log_tail(),
            );
            std::thread::sleep(POLL);
        }
    }

    /// Read `GET /+analytics/completeness` as the operator and parse the totals and the producer's
    /// accepted frontier, or `None` while the node is unreachable or answers non-200.
    fn completeness(&self) -> Option<Completeness> {
        let (code, body) = self.http_get_as("/+analytics/completeness")?;
        if code != 200 {
            return None;
        }
        let json: Value = serde_json::from_str(&body).ok()?;
        let totals = json.get("totals")?;
        let accepted = json
            .get("producers")?
            .as_array()?
            .iter()
            .find(|entry| entry.get("producer").and_then(Value::as_str) == Some(PRODUCER));
        Some(Completeness {
            downloads: totals.get("downloads")?.as_u64()?,
            bytes: totals.get("bytes")?.as_u64()?,
            accepted_epoch: accepted
                .and_then(|entry| entry.get("accepted_epoch"))
                .and_then(Value::as_u64),
            accepted_day: accepted
                .and_then(|entry| entry.get("accepted_day"))
                .and_then(Value::as_i64),
        })
    }

    fn http_get_as(&self, path: &str) -> Option<(u16, String)> {
        let response = self
            .http
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .basic_auth(ADMIN_USER, Some(ADMIN_PASSWORD))
            .send()
            .ok()?;
        let code = response.status().as_u16();
        Some((code, response.text().unwrap_or_default()))
    }

    fn restart(&mut self) {
        self.kill();
        self.child = launch(&self.config, self.data.path(), self.port);
        self.await_ready();
    }

    fn kill(&mut self) {
        kill_group(&mut self.child);
    }

    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(self.data.path().join("peryx.log")).unwrap_or_default();
        let mut lines: Vec<&str> = log.lines().rev().take(40).collect();
        lines.reverse();
        lines.join("\n")
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        kill_group(&mut self.child);
    }
}

/// Write the producer's sealed daily aggregate directly into its store before it boots. The producer
/// restores it as a sealed past day and re-exports it on the replication endpoint; the value is known
/// here, so the replica's convergence is asserted against it.
fn seed_sealed_day(data: &Path, day: i64) {
    let store = MetaStore::open(data.join("peryx.redb")).expect("open producer store");
    store
        .analytics()
        .save_daily(&encode_daily_snapshot(vec![DailyUsage {
            day,
            repository: "hosted".to_owned(),
            project: "veloxdemo".to_owned(),
            version: "1.0.0".to_owned(),
            source: String::new(),
            downloads: SEED_DOWNLOADS,
            bytes: SEED_BYTES,
        }]))
        .expect("seed sealed day");
}

fn producer_config(ports: &Ports) -> String {
    let mut config = identity_and_index(PRODUCER);
    config.push_str(&roster(ports));
    let _ = write!(
        config,
        "[availability.replication]\nrole = \"primary\"\nsource = \"{PRODUCER}\"\ntoken = \"{TOKEN}\"\n\n",
    );
    let _ = writeln!(
        config,
        "[availability.listener]\nbind = \"127.0.0.1:{}\"",
        ports.producer_control
    );
    config
}

fn replica_config(ports: &Ports) -> String {
    let mut config = identity_and_index(REPLICA);
    config.push_str(&roster(ports));
    let _ = write!(
        config,
        "[availability.replication]\nrole = \"replica\"\nupstream = \"http://127.0.0.1:{}\"\ntoken = \"{TOKEN}\"\n\n",
        ports.producer_public,
    );
    let _ = writeln!(
        config,
        "[availability.listener]\nbind = \"127.0.0.1:{}\"",
        ports.replica_control
    );
    config
}

/// The top-level identity keys (which must precede any table) and the single hosted index every node
/// serves. Both nodes follow the one writer identity; each names its own roster entry for consensus.
fn identity_and_index(node: &str) -> String {
    let mut config = format!("writer_identity = \"{PRODUCER}\"\nnode_identity = \"{node}\"\n\n");
    config.push_str("[[index]]\nname = \"hosted\"\nhosted = true\nvolatile = true\n\n");
    config
}

/// The `dc` mode selector and the two-member roster shared by both nodes, naming each node's control
/// address so the replica knows the producer is the writer it expects a sealed day from.
fn roster(ports: &Ports) -> String {
    let mut toml = format!("[availability]\nmode = \"dc\"\ngroup = \"{GROUP}\"\n\n");
    for (node, dc, role, control) in [
        (PRODUCER, "dc-a", "writer", ports.producer_control),
        (REPLICA, "dc-b", "replica", ports.replica_control),
    ] {
        let _ = write!(
            toml,
            "[[availability.member]]\nnode = \"{node}\"\ndc = \"{dc}\"\naddress = \"http://127.0.0.1:{control}\"\nrole = \"{role}\"\n\n",
        );
    }
    toml
}

/// Create the operator that reads completeness, offline, before the node serves, through the same
/// command an administrator runs.
fn bootstrap_admin(config: &Path, data: &Path) {
    let password_file = data.join("admin-password");
    std::fs::write(&password_file, ADMIN_PASSWORD).expect("write admin password");
    let output = Command::new(BIN)
        .arg("bootstrap-administrator")
        .arg(ADMIN_USER)
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .arg("--password-file")
        .arg(&password_file)
        .output()
        .expect("run peryx bootstrap-administrator");
    assert!(
        output.status.success(),
        "bootstrap-administrator failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Run an offline peryx subcommand against a node's store before it serves, failing with its stderr.
fn run_offline(config: &Path, data: &Path, args: &[&str]) {
    let output = Command::new(BIN)
        .args(args)
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .output()
        .unwrap_or_else(|error| panic!("run peryx {}: {error}", args.join(" ")));
    assert!(
        output.status.success(),
        "peryx {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn launch(config: &Path, data: &Path, port: u16) -> Child {
    let log = std::fs::File::create(data.join("peryx.log")).expect("create node log");
    let mut command = Command::new(BIN);
    command
        .arg("serve")
        .args(["--host", "127.0.0.1"])
        .args(["--port", &port.to_string()])
        .arg("--data-dir")
        .arg(data)
        .arg("--config")
        .arg(config)
        .args(["--log-level", "debug"])
        .stdout(log.try_clone().expect("clone log handle"))
        .stderr(log);
    spawn_in_group(&mut command);
    command.spawn().expect("spawn peryx")
}

fn spawn_in_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let _ = command;
}

fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("pid fits an i32"));
        let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

/// The current UTC day in whole days since the epoch, the same flooring the node's clock applies, so the
/// seeded `today - 1` is a day the producer has already sealed.
fn utc_today() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX));
    secs.div_euclid(SECONDS_PER_DAY)
}
