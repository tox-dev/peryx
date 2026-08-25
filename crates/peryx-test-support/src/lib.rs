//! The harness drives production binaries through public APIs. Each child owns a process group so
//! [`Drop`] reaps descendants after failures.

mod driver;
pub mod toxiproxy;

#[cfg(test)]
#[path = "../tests/unit.rs"]
mod tests;

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;

pub use driver::EcosystemDriverFixture;
pub use toxiproxy::{Proxy, Toxiproxy};

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const EVENT_TIMEOUT: Duration = Duration::from_secs(90);
const TEST_TMPDIR_ENV: &str = "PERYX_TEST_TMPDIR";
#[cfg(unix)]
const PUBLIC_LISTENER_FD_ENV: &str = "PERYX_INHERITED_PUBLIC_LISTENER_FD";
#[cfg(unix)]
const AVAILABILITY_LISTENER_FD_ENV: &str = "PERYX_INHERITED_AVAILABILITY_LISTENER_FD";
// Keep an individual request deadline inside the overall readiness deadline.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
type DataPreparation<'a> = &'a dyn Fn(&MemberSpec, &std::path::Path);

pub(crate) fn http_client(timeout: Duration) -> reqwest::blocking::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .expect("build HTTP client")
}

/// Credentials installed by [`Topology::with_admin`] for privileged endpoint tests.
pub const ADMIN_USER: &str = "harness-admin";
pub const ADMIN_PASSWORD: &str = "harness-admin-secret";

#[must_use]
/// Resolves a package binary for the current Cargo or Nextest integration test.
///
/// # Panics
/// Panics when neither test runner supplies the requested binary path.
pub fn cargo_binary(name: &str) -> PathBuf {
    PathBuf::from(
        std::env::var_os(format!("NEXTEST_BIN_EXE_{}", name.replace('-', "_")))
            .or_else(|| std::env::var_os(format!("CARGO_BIN_EXE_{name}")))
            .expect("Cargo or Nextest integration binary path"),
    )
}

/// Process settings for spawned `peryx` nodes.
#[derive(Debug, Clone)]
pub struct ProcessHarness {
    binary: PathBuf,
    ready_timeout: Duration,
    shutdown_path: Option<String>,
    process_limit: Option<ProcessLimit>,
}

impl Default for ProcessHarness {
    fn default() -> Self {
        Self {
            binary: peryx_binary(),
            ready_timeout: READY_TIMEOUT,
            shutdown_path: None,
            process_limit: None,
        }
    }
}

impl ProcessHarness {
    /// Use `binary` instead of resolving `PERYX_BIN` or `peryx` from `PATH`.
    #[must_use]
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            ready_timeout: READY_TIMEOUT,
            shutdown_path: None,
            process_limit: None,
        }
    }

    #[must_use]
    pub const fn with_ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_shutdown_path(mut self, path: impl Into<String>) -> Self {
        self.shutdown_path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_process_limit(mut self, limit: ProcessLimit) -> Self {
        self.process_limit = Some(limit);
        self
    }

    /// # Errors
    /// Returns the process startup failure.
    pub fn spawn_on_port(&self, identity: &str, port: u16) -> Result<Node, HarnessError> {
        Node::start_raw(identity, ListenerReservation::released(port), String::new(), self)
    }

    /// # Errors
    /// Returns the process startup failure.
    pub fn spawn_with_config(&self, identity: &str, config_toml: &str) -> Result<Node, HarnessError> {
        Node::start_raw(
            identity,
            ListenerReservation::ephemeral()?,
            config_toml.to_owned(),
            self,
        )
    }

    /// Start a process and return after its log emits `event`, without requiring readiness.
    ///
    /// # Errors
    /// Returns the process startup or event failure.
    pub fn spawn_until_event(&self, identity: &str, config_toml: &str, event: &str) -> Result<Node, HarnessError> {
        let node = Node::launch_raw(
            identity,
            ListenerReservation::ephemeral()?,
            config_toml.to_owned(),
            self,
        )?;
        node.await_event(event)?;
        Ok(node)
    }
}

/// A child-process concurrency limit shared across independent harnesses.
#[derive(Debug, Clone)]
pub struct ProcessLimit {
    inner: Arc<ProcessLimitInner>,
}

#[derive(Debug)]
struct ProcessLimitInner {
    available: Mutex<usize>,
    changed: Condvar,
}

impl ProcessLimit {
    /// # Panics
    /// Panics when `maximum` is zero.
    #[must_use]
    pub fn new(maximum: usize) -> Self {
        assert!(maximum > 0, "a process limit must allow at least one child");
        Self {
            inner: Arc::new(ProcessLimitInner {
                available: Mutex::new(maximum),
                changed: Condvar::new(),
            }),
        }
    }

    fn acquire(&self) -> ProcessPermit {
        let mut available = self.inner.available.lock().expect("process limit mutex poisoned");
        while *available == 0 {
            available = self
                .inner
                .changed
                .wait(available)
                .expect("process limit mutex poisoned");
        }
        *available -= 1;
        drop(available);
        ProcessPermit { limit: self.clone() }
    }
}

#[derive(Debug)]
struct ProcessPermit {
    limit: ProcessLimit,
}

impl Drop for ProcessPermit {
    fn drop(&mut self) {
        *self.limit.inner.available.lock().expect("process limit mutex poisoned") += 1;
        self.limit.inner.changed.notify_one();
    }
}

/// A harness failure, distinct from a node's own error, so a self-test can assert why the harness gave
/// up rather than only that it did.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("node {node:?} did not become ready within {timeout:?}\n--- log tail ---\n{log}")]
    NotReady {
        node: String,
        timeout: Duration,
        log: String,
    },
    #[error("node {node:?} exited during startup with {status}\n--- log tail ---\n{log}")]
    ExitedEarly { node: String, status: String, log: String },
    #[error("toxiproxy: {0}")]
    Toxiproxy(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("peryx rejected the generated config:\n{0}")]
    Config(String),
    #[error("authority did not leave {from:?} within {within:?}; it still reports {observed:?}")]
    NoTransfer {
        from: String,
        within: Duration,
        observed: Option<String>,
    },
    #[error("no authority leader emerged within {within:?}")]
    NoLeader { within: Duration },
    #[error("node {node:?} emitted no matching topology signal within {within:?}; last observation: {last}")]
    SignalTimeout {
        node: String,
        within: Duration,
        last: String,
    },
    #[error("node {node:?} closed its topology stream; last observation: {last}")]
    SignalClosed { node: String, last: String },
    #[error("node {node:?} topology stream failed: {failure}; last observation: {last}")]
    SignalRead {
        node: String,
        failure: String,
        last: String,
    },
}

/// The availability mode a node runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Single node, no replication: the zero-config default.
    None,
    /// A writer and read replicas within one datacenter.
    Dc,
    /// Metadata durability across datacenters, running the embedded ownership Raft node.
    Ha,
}

/// The role a member plays in a `dc` or `ha` group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Writer,
    Replica,
}

impl Role {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Writer => "writer",
            Self::Replica => "replica",
        }
    }
}

/// One member of a topology, before ports are assigned.
#[derive(Debug, Clone)]
pub struct MemberSpec {
    pub node: String,
    pub dc: String,
    pub role: Role,
}

impl MemberSpec {
    #[must_use]
    pub fn new(node: &str, dc: &str, role: Role) -> Self {
        Self {
            node: node.to_owned(),
            dc: dc.to_owned(),
            role,
        }
    }
}

/// A cluster blueprint: the mode, group name, shared peer token, and the member roster.
#[derive(Debug, Clone)]
pub struct Topology {
    mode: Mode,
    group: String,
    token: String,
    members: Vec<MemberSpec>,
    bootstrap_admin: bool,
    index_config: String,
    write_ack_deadline_secs: Option<u64>,
    harness: ProcessHarness,
}

impl Topology {
    #[must_use]
    pub fn single() -> Self {
        Self {
            mode: Mode::None,
            group: "solo".to_owned(),
            token: "harness-token".to_owned(),
            members: vec![MemberSpec::new("node-a", "local", Role::Writer)],
            bootstrap_admin: false,
            index_config: String::new(),
            write_ack_deadline_secs: None,
            harness: ProcessHarness::default(),
        }
    }

    #[must_use]
    pub fn ha(group: &str, members: Vec<MemberSpec>) -> Self {
        Self {
            mode: Mode::Ha,
            group: group.to_owned(),
            token: "harness-token".to_owned(),
            members,
            bootstrap_admin: false,
            index_config: String::new(),
            write_ack_deadline_secs: None,
            harness: ProcessHarness::default(),
        }
    }

    #[must_use]
    pub fn dc(group: &str, members: Vec<MemberSpec>) -> Self {
        Self {
            mode: Mode::Dc,
            group: group.to_owned(),
            token: "harness-token".to_owned(),
            members,
            bootstrap_admin: false,
            index_config: String::new(),
            write_ack_deadline_secs: None,
            harness: ProcessHarness::default(),
        }
    }

    /// Bootstrap [`ADMIN_USER`] into every node before it serves, so a test can read the operator- and
    /// administrator-class fields of the observability surfaces through [`Node::http_get_as`]. Each
    /// node holds its own store, so each is bootstrapped independently with the same credential.
    #[must_use]
    pub const fn with_admin(mut self) -> Self {
        self.bootstrap_admin = true;
        self
    }

    #[must_use]
    pub fn with_index_config(mut self, config: &str) -> Self {
        self.index_config.push_str(config);
        if !self.index_config.ends_with('\n') {
            self.index_config.push('\n');
        }
        self
    }

    #[must_use]
    pub const fn with_write_ack_deadline(mut self, seconds: u64) -> Self {
        self.write_ack_deadline_secs = Some(seconds);
        self
    }

    #[must_use]
    pub fn with_process_harness(mut self, harness: ProcessHarness) -> Self {
        self.harness = harness;
        self
    }

    #[must_use]
    pub fn with_replication_token(mut self, token: &str) -> Self {
        token.clone_into(&mut self.token);
        self
    }

    /// Spawn every member and wait until each answers `/+status`.
    ///
    /// # Errors
    /// Returns the first [`HarnessError`] a node reports while coming up.
    pub fn start(&self) -> Result<Cluster, HarnessError> {
        self.spawn_once(None)
    }

    /// # Errors
    /// Returns the first [`HarnessError`] a node reports while coming up.
    pub fn start_with_data(&self, prepare: impl Fn(&MemberSpec, &std::path::Path)) -> Result<Cluster, HarnessError> {
        self.spawn_once(Some(&prepare))
    }

    /// One attempt at spawning the cluster on a fresh port draw. A partial cluster whose later member
    /// fails is dropped here, so [`Node`]'s own `Drop` kills every process it already started.
    fn spawn_once(&self, prepare: Option<DataPreparation<'_>>) -> Result<Cluster, HarnessError> {
        let listeners = self
            .members
            .iter()
            .map(|_| NodeListeners::ephemeral(self.mode != Mode::None))
            .collect::<Result<Vec<_>, _>>()?;
        let addresses = listeners.iter().map(NodeListeners::ports).collect::<Vec<_>>();
        let roster = self.roster_toml(&addresses);
        let writer = (self.mode != Mode::None).then(|| self.writer(&addresses));
        let mut nodes = Vec::with_capacity(self.members.len());
        for (member, listeners) in self.members.iter().zip(listeners) {
            let node = Node::spawn(self, member, listeners, writer.as_ref(), None, &roster, prepare)?;
            nodes.push(node);
        }
        Ok(Cluster { nodes })
    }

    /// Each replica gets a separate proxy so one writer link can fail without affecting its peers.
    ///
    /// Only meaningful for a `dc`/`ha` topology, the one shape with a writer replicas follow.
    ///
    /// # Errors
    /// The first [`HarnessError`] a node or the proxy server reports while coming up.
    pub fn start_proxied(&self, toxiproxy: &mut Toxiproxy, healthy: bool) -> Result<Proxied, HarnessError> {
        self.spawn_proxied(toxiproxy, healthy)
    }

    fn spawn_proxied(&self, toxiproxy: &mut Toxiproxy, healthy: bool) -> Result<Proxied, HarnessError> {
        let listeners = self
            .members
            .iter()
            .map(|_| NodeListeners::ephemeral(true))
            .collect::<Result<Vec<_>, _>>()?;
        let addresses = listeners.iter().map(NodeListeners::ports).collect::<Vec<_>>();
        let roster = self.roster_toml(&addresses);
        let writer = self.writer(&addresses);
        let mut nodes = Vec::with_capacity(self.members.len());
        let mut proxies = std::collections::HashMap::new();
        for (member, listeners) in self.members.iter().zip(listeners) {
            let upstream = if matches!(member.role, Role::Replica) {
                let proxy = toxiproxy.proxy(&format!("127.0.0.1:{}", writer.1))?;
                if !healthy {
                    proxy.partition()?;
                }
                let base = format!("http://{}", proxy.endpoint());
                proxies.insert(member.node.clone(), proxy);
                Some(base)
            } else {
                None
            };
            let node = Node::spawn(
                self,
                member,
                listeners,
                Some(&writer),
                upstream.as_deref(),
                &roster,
                None,
            )?;
            nodes.push(node);
        }
        Ok(Proxied {
            cluster: Cluster { nodes },
            proxies,
        })
    }

    /// Use this when generated-config validation does not require a running consensus group.
    ///
    /// # Errors
    /// [`HarnessError::Config`] with the validator's output when peryx rejects the generated config.
    ///
    /// # Panics
    /// Panics if the topology has no members. Public constructors prevent that state.
    pub fn validate_config(&self) -> Result<String, HarnessError> {
        let addresses: Vec<(u16, u16)> = self
            .members
            .iter()
            .enumerate()
            .map(|(index, _)| {
                (
                    9000 + 2 * u16::try_from(index).unwrap_or(0),
                    9001 + 2 * u16::try_from(index).unwrap_or(0),
                )
            })
            .collect();
        let roster = self.roster_toml(&addresses);
        let writer = (self.mode != Mode::None).then(|| self.writer(&addresses));
        let member = self.members.first().expect("a topology has at least one member");
        let dir = TempDir::new()?;
        let config = dir.path().join("peryx.toml");
        let contents = node_config(self, member, addresses[0].1, writer.as_ref(), None, &roster);
        std::fs::write(&config, contents)?;
        let output = Command::new(&self.harness.binary)
            .args(["config", "check"])
            .arg("--config")
            .arg(&config)
            .arg("--data-dir")
            .arg(dir.path())
            .output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(HarnessError::Config(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    /// The `[availability]` mode selector and the `[[availability.member]]` roster shared by every
    /// node. The per-process replication role lives in [`node_config`], not here, so a replica follows
    /// the writer rather than every node running as its own primary. Empty for a `none`-mode topology.
    fn roster_toml(&self, addresses: &[(u16, u16)]) -> String {
        let mode = match self.mode {
            Mode::None => return String::new(),
            Mode::Dc => "dc",
            Mode::Ha => "ha",
        };
        let mut toml = format!("[availability]\nmode = \"{}\"\ngroup = \"{}\"\n\n", mode, self.group);
        for (member, &(_, control)) in self.members.iter().zip(addresses) {
            let _ = write!(
                toml,
                "[[availability.member]]\nnode = \"{}\"\ndc = \"{}\"\naddress = \"http://127.0.0.1:{control}\"\nrole = \"{}\"\n\n",
                member.node,
                member.dc,
                member.role.as_str(),
            );
        }
        toml
    }

    fn writer(&self, addresses: &[(u16, u16)]) -> (String, u16) {
        self.members
            .iter()
            .zip(addresses)
            .find(|(member, _)| matches!(member.role, Role::Writer))
            .map(|(member, &(public, _))| (member.node.clone(), public))
            .expect("a dc or ha topology has a writer")
    }
}

/// A cluster with an independently controllable proxy between each replica and its writer.
/// Proxies remain owned by the caller's [`Toxiproxy`] instance.
pub struct Proxied {
    cluster: Cluster,
    proxies: std::collections::HashMap<String, Proxy>,
}

impl Proxied {
    #[must_use]
    pub const fn cluster(&self) -> &Cluster {
        &self.cluster
    }

    pub const fn cluster_mut(&mut self) -> &mut Cluster {
        &mut self.cluster
    }

    #[must_use]
    pub fn proxy(&self, replica: &str) -> Option<&Proxy> {
        self.proxies.get(replica)
    }
}

/// A spawned cluster. Dropping it kills every node's process group and removes its data directory.
pub struct Cluster {
    nodes: Vec<Node>,
}

impl Cluster {
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn nodes_mut(&mut self) -> &mut [Node] {
        &mut self.nodes
    }

    #[must_use]
    pub fn node(&self, identity: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.identity == identity)
    }

    #[must_use]
    pub fn failure_report(&self) -> FailureReport {
        FailureReport {
            nodes: self.nodes.iter().map(Node::snapshot).collect(),
        }
    }

    /// # Errors
    /// [`HarnessError::NoLeader`] when no leader emerges before the deadline.
    pub fn await_leader(&self, within: Duration) -> Result<String, HarnessError> {
        self.await_observed_leader(within, |_| true)
            .map_err(|_| HarnessError::NoLeader { within })
    }

    /// # Errors
    /// [`HarnessError::NoTransfer`] when `from` still holds authority at the deadline.
    pub fn await_leader_change(&self, from: &str, within: Duration) -> Result<String, HarnessError> {
        self.await_observed_leader(within, |leader| leader != from)
            .map_err(|observed| HarnessError::NoTransfer {
                from: from.to_owned(),
                within,
                observed,
            })
    }

    fn await_observed_leader(&self, within: Duration, accept: impl Fn(&str) -> bool) -> Result<String, Option<String>> {
        self.await_topology_signal(within, |cluster| {
            let observed = cluster.observed_leader();
            (
                observed.as_deref().filter(|leader| accept(leader)).map(str::to_owned),
                format!("last observed leader: {observed:?}"),
            )
        })
        .map_err(|_| self.observed_leader())
    }

    /// Wait for `observe` to accept state after a topology stream event.
    ///
    /// # Errors
    /// Returns a signal error when no live node can provide an event before the deadlock guard.
    pub fn await_topology_event<T>(
        &self,
        observe: impl FnMut(&Self) -> (Option<T>, String),
    ) -> Result<T, HarnessError> {
        self.await_topology_signal(EVENT_TIMEOUT, observe)
    }

    /// Wait for `observe` to accept state after a topology stream event.
    ///
    /// # Errors
    /// Returns a signal error when no live node can provide an event before `within`.
    pub fn await_topology_signal<T>(
        &self,
        within: Duration,
        mut observe: impl FnMut(&Self) -> (Option<T>, String),
    ) -> Result<T, HarnessError> {
        let (value, last) = observe(self);
        if let Some(value) = value {
            return Ok(value);
        }
        let Some(node) = self.nodes.iter().find(|node| process_alive(node.child.id())) else {
            return Err(HarnessError::SignalClosed {
                node: "cluster".to_owned(),
                last,
            });
        };
        node.await_signal(within, last, || observe(self))
    }
}

impl Cluster {
    /// The control-plane leader datacenter a quorum of nodes agrees on, or `None` until they concur.
    ///
    /// Polls every node's status surface and tallies the `consensus.leader` each reports, since a single
    /// node's view lags the group by a heartbeat and flaps mid-election. Agreement across a majority is
    /// the settled leader.
    fn observed_leader(&self) -> Option<String> {
        let mut tally: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for node in &self.nodes {
            let Some((200, body)) = node.control_get_as(ADMIN_USER, ADMIN_PASSWORD, "/availability/v1/status") else {
                continue;
            };
            let Ok(status) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            if let Some(leader) = status
                .get("consensus")
                .and_then(|consensus| consensus.get("leader"))
                .and_then(serde_json::Value::as_str)
            {
                *tally.entry(leader.to_owned()).or_default() += 1;
            }
        }
        let quorum = self.nodes.len() / 2 + 1;
        tally
            .into_iter()
            .find(|(_, count)| *count >= quorum)
            .map(|(leader, _)| leader)
    }
}

impl OwnershipControl for Cluster {
    fn leader(&self) -> Result<Option<String>, HarnessError> {
        Ok(self.observed_leader())
    }

    fn await_authority_transfer(&self, from: &str, within: Duration) -> Result<String, HarnessError> {
        self.await_leader_change(from, within)
    }
}

/// One running `peryx serve` process and the surface a test drives it through.
#[derive(Debug)]
pub struct Node {
    identity: String,
    child: Child,
    port: u16,
    control_port: u16,
    config: PathBuf,
    data: TempDir,
    http: reqwest::blocking::Client,
    binary: PathBuf,
    ready_timeout: Duration,
    shutdown_path: Option<String>,
    process_events: Receiver<String>,
    ready: bool,
    _process_permit: Option<ProcessPermit>,
}

impl Node {
    fn spawn(
        topology: &Topology,
        member: &MemberSpec,
        listeners: NodeListeners,
        writer: Option<&(String, u16)>,
        upstream: Option<&str>,
        roster: &str,
        prepare: Option<DataPreparation<'_>>,
    ) -> Result<Self, HarnessError> {
        let (port, control_port) = listeners.ports();
        let data = node_temp_dir()?;
        let config = data.path().join("peryx.toml");
        std::fs::write(
            &config,
            node_config(topology, member, control_port, writer, upstream, roster),
        )
        .expect("write config");
        if let Some(prepare) = prepare {
            prepare(member, data.path());
        }
        if topology.bootstrap_admin {
            bootstrap_admin(&config, data.path(), &topology.harness.binary)?;
        }
        // A replica starts read-only and only verifies the writer identity, so its store must already
        // hold it: seed it offline through the same binary before serving.
        if writer.is_some() && !matches!(member.role, Role::Writer) {
            claim_writer_identity(&config, data.path(), &topology.harness.binary)?;
        }
        let http = http_client(HTTP_TIMEOUT.min(topology.harness.ready_timeout));
        let process_permit = topology.harness.process_limit.as_ref().map(ProcessLimit::acquire);
        let (child, process_events) = launch(&config, data.path(), port, &topology.harness.binary, listeners)?;
        let mut node = Self {
            identity: member.node.clone(),
            child,
            port,
            control_port,
            config,
            data,
            http,
            binary: topology.harness.binary.clone(),
            ready_timeout: topology.harness.ready_timeout,
            shutdown_path: topology.harness.shutdown_path.clone(),
            process_events,
            ready: false,
            _process_permit: process_permit,
        };
        node.await_ready()?;
        Ok(node)
    }

    /// # Errors
    /// [`HarnessError::ExitedEarly`] when the child dies first, [`HarnessError::NotReady`] on timeout.
    pub fn await_ready(&mut self) -> Result<(), HarnessError> {
        if self.ready {
            return self.ready_observation();
        }
        match wait_for_startup(&mut self.child, &self.process_events, self.ready_timeout, |line| {
            line.contains("peryx listening")
        })? {
            StartupSignal::Matched => {
                let observation = self.ready_observation();
                self.ready = observation.is_ok();
                observation
            }
            StartupSignal::Exited(status) => Err(self.exited_early(status)),
            StartupSignal::TimedOut => Err(self.not_ready(Err("startup signal missing".to_owned()))),
        }
    }

    fn ready_observation(&self) -> Result<(), HarnessError> {
        let observation = self.observe_status(HTTP_TIMEOUT.min(self.ready_timeout));
        if matches!(&observation, Ok((200, body)) if body.contains("\"version\"")) {
            return Ok(());
        }
        Err(self.not_ready(observation))
    }

    fn observe_status(&self, timeout: Duration) -> Result<(u16, String), String> {
        let response = http_client(timeout)
            .get(format!("http://127.0.0.1:{}/+status", self.port))
            .send()
            .map_err(|error| error.to_string())?;
        let code = response.status().as_u16();
        let body = response.text().unwrap_or_default();
        Ok((code, body))
    }

    fn exited_early(&self, status: std::process::ExitStatus) -> HarnessError {
        HarnessError::ExitedEarly {
            node: self.identity.clone(),
            status: status.to_string(),
            log: startup_log(&self.log()),
        }
    }

    fn not_ready(&self, observation: Result<(u16, String), String>) -> HarnessError {
        HarnessError::NotReady {
            node: self.identity.clone(),
            timeout: self.ready_timeout,
            log: format!(
                "last observation: {}\nprocess: running (pid {})\n{}",
                observation.map_or_else(|error| error, |(code, body)| format!("HTTP {code}: {body}")),
                self.child.id(),
                self.log_tail(),
            ),
        }
    }

    fn start_raw(
        identity: &str,
        public_listener: ListenerReservation,
        config_toml: String,
        harness: &ProcessHarness,
    ) -> Result<Self, HarnessError> {
        let mut node = Self::launch_raw(identity, public_listener, config_toml, harness)?;
        node.await_ready()?;
        Ok(node)
    }

    fn launch_raw(
        identity: &str,
        public_listener: ListenerReservation,
        config_toml: String,
        harness: &ProcessHarness,
    ) -> Result<Self, HarnessError> {
        let port = public_listener.port;
        let data = node_temp_dir()?;
        let config = data.path().join("peryx.toml");
        std::fs::write(&config, config_toml).expect("write config");
        let http = http_client(HTTP_TIMEOUT.min(harness.ready_timeout));
        let process_permit = harness.process_limit.as_ref().map(ProcessLimit::acquire);
        let (child, process_events) = launch(
            &config,
            data.path(),
            port,
            &harness.binary,
            NodeListeners::public(public_listener),
        )?;
        Ok(Self {
            identity: identity.to_owned(),
            child,
            port,
            control_port: 0,
            config,
            data,
            http,
            binary: harness.binary.clone(),
            ready_timeout: harness.ready_timeout,
            shutdown_path: harness.shutdown_path.clone(),
            process_events,
            ready: false,
            _process_permit: process_permit,
        })
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    #[must_use]
    pub fn control_endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.control_port)
    }

    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    #[must_use]
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status()
            .is_some_and(|(code, body)| code == 200 && body.contains("\"version\""))
    }

    #[must_use]
    pub fn status(&self) -> Option<(u16, String)> {
        self.http_get("/+status")
    }

    #[must_use]
    pub fn readiness(&self) -> Option<(u16, String)> {
        self.http_get("/+ready")
    }

    #[must_use]
    pub fn topology(&self) -> Option<(u16, String)> {
        self.http_get("/+availability/topology")
    }

    #[must_use]
    pub fn metrics(&self) -> Option<(u16, String)> {
        self.http_get("/metrics")
    }

    #[must_use]
    pub fn placements(&self) -> Option<(u16, String)> {
        self.http_get("/+availability/placements")
    }

    #[must_use]
    pub fn http_get(&self, path: &str) -> Option<(u16, String)> {
        let response = self
            .http
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .send()
            .ok()?;
        let code = response.status().as_u16();
        Some((code, response.text().unwrap_or_default()))
    }

    #[must_use]
    pub fn http_get_as(&self, user: &str, password: &str, path: &str) -> Option<(u16, String)> {
        let response = self
            .http
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .basic_auth(user, Some(password))
            .send()
            .ok()?;
        let code = response.status().as_u16();
        Some((code, response.text().unwrap_or_default()))
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        self.http
            .request(method, format!("http://127.0.0.1:{}{path}", self.port))
    }

    /// Wait for `observe` to accept state after a topology stream event.
    ///
    /// # Errors
    /// Returns a signal error when the stream closes or misses the deadlock guard.
    pub fn await_topology_event<T>(
        &self,
        observe: impl FnMut(&Self) -> (Option<T>, String),
    ) -> Result<T, HarnessError> {
        self.await_topology_signal(EVENT_TIMEOUT, observe)
    }

    /// Wait for `observe` to accept state after a topology stream event.
    ///
    /// # Errors
    /// Returns a signal error when the stream closes or misses `within`.
    pub fn await_topology_signal<T>(
        &self,
        within: Duration,
        mut observe: impl FnMut(&Self) -> (Option<T>, String),
    ) -> Result<T, HarnessError> {
        let (value, last) = observe(self);
        value.map_or_else(|| self.await_signal(within, last, || observe(self)), Ok)
    }

    fn await_signal<T>(
        &self,
        within: Duration,
        mut last: String,
        mut observe: impl FnMut() -> (Option<T>, String),
    ) -> Result<T, HarnessError> {
        let response = http_client(within)
            .get(format!("http://127.0.0.1:{}/+availability/topology/stream", self.port))
            .send()
            .map_err(|error| HarnessError::SignalRead {
                node: self.identity.clone(),
                failure: error.to_string(),
                last: last.clone(),
            })?;
        if !response.status().is_success() {
            return Err(HarnessError::SignalRead {
                node: self.identity.clone(),
                failure: format!("HTTP {}", response.status()),
                last,
            });
        }
        let mut reader = BufReader::new(response);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    return Err(HarnessError::SignalClosed {
                        node: self.identity.clone(),
                        last,
                    });
                }
                Ok(_) if line == "\n" || line == "\r\n" => {
                    let (value, failure) = observe();
                    last = failure;
                    if let Some(value) = value {
                        return Ok(value);
                    }
                }
                Ok(_) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::TimedOut
                        || error
                            .get_ref()
                            .and_then(|source| source.downcast_ref::<reqwest::Error>())
                            .is_some_and(reqwest::Error::is_timeout) =>
                {
                    return Err(HarnessError::SignalTimeout {
                        node: self.identity.clone(),
                        within,
                        last,
                    });
                }
                Err(error) => {
                    return Err(HarnessError::SignalRead {
                        node: self.identity.clone(),
                        failure: error.to_string(),
                        last,
                    });
                }
            }
        }
    }

    #[must_use]
    pub fn download(&self, path: &str) -> Option<(u16, Vec<u8>)> {
        let response = self
            .http
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .send()
            .ok()?;
        let code = response.status().as_u16();
        Some((code, response.bytes().ok()?.to_vec()))
    }

    /// Availability control data uses the authenticated control listener instead of the public port.
    #[must_use]
    pub fn control_get_as(&self, user: &str, password: &str, path: &str) -> Option<(u16, String)> {
        let response = self
            .http
            .get(format!("http://127.0.0.1:{}{path}", self.control_port))
            .basic_auth(user, Some(password))
            .send()
            .ok()?;
        let code = response.status().as_u16();
        Some((code, response.text().unwrap_or_default()))
    }

    pub fn kill(&mut self) {
        self.stop();
    }

    /// Preserves data and listener addresses so tests observe recovery instead of fresh state.
    ///
    /// # Errors
    /// The [`HarnessError`] the fresh process reports while coming up.
    pub fn restart(&mut self) -> Result<(), HarnessError> {
        self.kill();
        let (child, process_events) = launch(
            &self.config,
            self.data.path(),
            self.port,
            &self.binary,
            NodeListeners::on_ports(self.port, self.control_port)?,
        )?;
        self.child = child;
        self.process_events = process_events;
        self.ready = false;
        self.await_ready()
    }

    /// Wait for a child log event containing `expected`, including an event already persisted.
    ///
    /// # Errors
    /// Returns a signal error when the child exits or misses the deadline.
    pub fn await_event(&self, expected: &str) -> Result<(), HarnessError> {
        self.await_log_signal(EVENT_TIMEOUT, expected)
    }

    /// Wait for a child log event containing `expected`, including an event already persisted.
    ///
    /// # Errors
    /// Returns a signal error when the child exits or misses `within`.
    pub fn await_log_signal(&self, within: Duration, expected: &str) -> Result<(), HarnessError> {
        if self.log().contains(expected) {
            return Ok(());
        }
        let last = || format!("expected log event {expected:?}\n{}", self.log_tail());
        match wait_for_line(&self.process_events, within, |line| line.contains(expected)) {
            ProcessSignal::Matched => Ok(()),
            _ if self.log().contains(expected) => Ok(()),
            ProcessSignal::Closed => Err(HarnessError::SignalClosed {
                node: self.identity.clone(),
                last: last(),
            }),
            ProcessSignal::TimedOut => Err(HarnessError::SignalTimeout {
                node: self.identity.clone(),
                within,
                last: last(),
            }),
        }
    }

    #[must_use]
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.data.path().join("peryx.log")).unwrap_or_default()
    }

    #[must_use]
    pub fn log_tail(&self) -> String {
        self.log()
            .lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[must_use]
    pub fn diagnostics(&self) -> String {
        format!(
            "process: {} (pid {})\nlog:\n{}",
            if process_alive(self.child.id()) {
                "running"
            } else {
                "not running"
            },
            self.child.id(),
            self.log_tail(),
        )
    }

    fn snapshot(&self) -> NodeArtifact {
        NodeArtifact {
            identity: self.identity.clone(),
            process: if process_alive(self.child.id()) {
                format!("running (pid {})", self.child.id())
            } else {
                format!("not running (pid {})", self.child.id())
            },
            topology: self.topology().map(|(_, body)| body),
            status: self.status().map(|(_, body)| body),
            log_tail: self.log_tail(),
        }
    }

    fn stop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Some(path) = &self.shutdown_path
            && self
                .http
                .get(format!("http://127.0.0.1:{}{path}", self.port))
                .send()
                .is_ok()
        {
            let _ = self.child.wait();
            return;
        }
        kill_group(&mut self.child);
    }
}

fn startup_log(log: &str) -> String {
    const HEAD_LINES: usize = 20;
    const TAIL_LINES: usize = 40;

    let lines: Vec<_> = log.lines().collect();
    if lines.len() <= HEAD_LINES + TAIL_LINES {
        return lines.join("\n");
    }
    format!(
        "{}\n... {} lines omitted ...\n{}",
        lines[..HEAD_LINES].join("\n"),
        lines.len() - HEAD_LINES - TAIL_LINES,
        lines[lines.len() - TAIL_LINES..].join("\n")
    )
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

fn node_temp_dir() -> Result<TempDir, HarnessError> {
    if let Some(root) = std::env::var_os(TEST_TMPDIR_ENV) {
        std::fs::create_dir_all(&root)?;
        return Ok(tempfile::Builder::new().prefix("peryx-node-").tempdir_in(root)?);
    }
    Ok(TempDir::new()?)
}

/// A diagnostic bundle for a failed test: one entry per node.
#[derive(Debug)]
pub struct FailureReport {
    pub nodes: Vec<NodeArtifact>,
}

impl FailureReport {
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for node in &self.nodes {
            let _ = write!(
                out,
                "== node {} ==\nprocess: {}\ntopology: {}\nstatus: {}\nlog:\n{}\n\n",
                node.identity,
                node.process,
                node.topology.as_deref().unwrap_or("<unreachable>"),
                node.status.as_deref().unwrap_or("<unreachable>"),
                node.log_tail,
            );
        }
        out
    }
}

/// One node's slice of a [`FailureReport`]: its topology and status (the pending-operations surface),
/// plus the tail of its log.
#[derive(Debug)]
pub struct NodeArtifact {
    pub identity: String,
    pub process: String,
    pub topology: Option<String>,
    pub status: Option<String>,
    pub log_tail: String,
}

/// The ownership-plane controls used by failover tests.
pub trait OwnershipControl {
    /// The datacenter currently holding control-plane authority, or `None` until a quorum agrees on one.
    ///
    /// # Errors
    /// [`HarnessError`] when the status surface cannot be read.
    fn leader(&self) -> Result<Option<String>, HarnessError>;

    /// Wait until authority leaves `from` within `within`, returning the datacenter that took it.
    ///
    /// # Errors
    /// [`HarnessError::NoTransfer`] when `from` still holds authority at the deadline.
    fn await_authority_transfer(&self, from: &str, within: Duration) -> Result<String, HarnessError>;
}

fn node_config(
    topology: &Topology,
    member: &MemberSpec,
    control_port: u16,
    writer: Option<&(String, u16)>,
    upstream: Option<&str>,
    roster: &str,
) -> String {
    // Top-level keys must precede any table, or TOML folds them into the last `[[index]]`.
    let mut config = String::new();
    if let Some(writer) = writer {
        let _ = writeln!(config, "writer_identity = \"{}\"", writer.0);
        if topology.mode == Mode::Ha {
            // Consensus requires each process to claim its own roster entry.
            let _ = writeln!(config, "node_identity = \"{}\"", member.node);
        }
        config.push('\n');
    }
    config.push_str(&topology.index_config);
    if let Some(writer) = writer {
        config.push_str(roster);
        if let Some(seconds) = topology.write_ack_deadline_secs {
            let _ = writeln!(config, "[availability.write_ack]\ndeadline-secs = {seconds}\n");
        }
        if matches!(member.role, Role::Writer) {
            let _ = write!(
                config,
                "[availability.replication]\nrole = \"primary\"\nsource = \"{}\"\ntoken = \"{}\"\n\n",
                writer.0, topology.token,
            );
        } else {
            let base = upstream.map_or_else(|| format!("http://127.0.0.1:{}", writer.1), str::to_owned);
            let _ = write!(
                config,
                "[availability.replication]\nrole = \"replica\"\nupstream = \"{base}\"\ntoken = \"{}\"\n\n",
                topology.token,
            );
        }
        let _ = writeln!(config, "[availability.listener]\nbind = \"127.0.0.1:{control_port}\"");
    }
    config
}

fn bootstrap_admin(
    config: &std::path::Path,
    data: &std::path::Path,
    binary: &std::path::Path,
) -> Result<(), HarnessError> {
    let password_file = data.join("admin-password");
    std::fs::write(&password_file, ADMIN_PASSWORD)?;
    let output = Command::new(binary)
        .arg("bootstrap-administrator")
        .arg(ADMIN_USER)
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .arg("--password-file")
        .arg(&password_file)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(HarnessError::Config(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

/// Seed a replica's store with the writer identity it follows, through the same offline command an
/// operator runs, so the replica passes its read-only startup identity check before it serves.
fn claim_writer_identity(
    config: &std::path::Path,
    data: &std::path::Path,
    binary: &std::path::Path,
) -> Result<(), HarnessError> {
    let output = Command::new(binary)
        .args(["writer", "claim"])
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .output()
        .expect("run peryx writer claim");
    if output.status.success() {
        Ok(())
    } else {
        Err(HarnessError::Config(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

fn launch(
    config: &std::path::Path,
    data: &std::path::Path,
    port: u16,
    binary: &std::path::Path,
    listeners: NodeListeners,
) -> std::io::Result<(Child, Receiver<String>)> {
    let log_path = data.join("peryx.log");
    let log = std::fs::File::create(&log_path).expect("create node log");
    let mut command = inherited_listener_command(&resolve_executable(binary)?, listeners, log);
    command
        .arg("serve")
        .args(["--host", "127.0.0.1"])
        .args(["--port", &port.to_string()])
        .arg("--data-dir")
        .arg(data)
        .arg("--config")
        .arg(config)
        .args(["--log-level", "info"]);
    spawn_in_group(&mut command);
    spawn_with_events(&mut command, Some(&log_path), false)
}

fn resolve_executable(binary: &Path) -> std::io::Result<PathBuf> {
    if binary.components().count() > 1 {
        std::fs::metadata(binary)?;
        return Ok(binary.to_owned());
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(binary))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{} not found", binary.display())))
}

pub(crate) fn spawn_with_events(
    command: &mut Command,
    log: Option<&Path>,
    capture_stderr: bool,
) -> std::io::Result<(Child, Receiver<String>)> {
    command.stdout(Stdio::piped());
    if capture_stderr {
        command.stderr(Stdio::piped());
    }
    let mut child = command.spawn()?;
    let (sender, receiver) = mpsc::channel();
    capture_lines(child.stdout.take().expect("captured child stdout"), log, sender.clone())?;
    if capture_stderr {
        capture_lines(child.stderr.take().expect("captured child stderr"), log, sender.clone())?;
    }
    drop(sender);
    Ok((child, receiver))
}

fn capture_lines(
    stream: impl std::io::Read + Send + 'static,
    log: Option<&Path>,
    sender: mpsc::Sender<String>,
) -> std::io::Result<()> {
    let mut log = log
        .map(|path| std::fs::OpenOptions::new().append(true).open(path))
        .transpose()?;
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Some(log) = &mut log {
                let _ = writeln!(log, "{line}");
                let _ = log.flush();
            }
            let _ = sender.send(line);
        }
    });
    Ok(())
}

pub(crate) enum ProcessSignal {
    Matched,
    Closed,
    TimedOut,
}

pub(crate) enum StartupSignal {
    Matched,
    Exited(std::process::ExitStatus),
    TimedOut,
}

pub(crate) fn wait_for_startup(
    child: &mut Child,
    events: &Receiver<String>,
    timeout: Duration,
    accept: impl Fn(&str) -> bool,
) -> std::io::Result<StartupSignal> {
    match wait_for_line(events, timeout, accept) {
        ProcessSignal::Matched => Ok(StartupSignal::Matched),
        ProcessSignal::Closed => child.wait().map(StartupSignal::Exited),
        ProcessSignal::TimedOut => Ok(child.try_wait()?.map_or(StartupSignal::TimedOut, StartupSignal::Exited)),
    }
}

pub(crate) fn wait_for_line(
    events: &Receiver<String>,
    timeout: Duration,
    accept: impl Fn(&str) -> bool,
) -> ProcessSignal {
    let deadline = Instant::now() + timeout;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) if accept(&line) => return ProcessSignal::Matched,
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => return ProcessSignal::Closed,
            Err(RecvTimeoutError::Timeout) => return ProcessSignal::TimedOut,
        }
    }
}

#[cfg(unix)]
fn inherited_listener_command(binary: &Path, listeners: NodeListeners, log: std::fs::File) -> Command {
    use std::os::fd::OwnedFd;
    use std::process::Stdio;

    let public = listeners.public.listener;
    let availability = listeners.availability.listener;
    if public.is_none() && availability.is_none() {
        let mut command = Command::new(binary);
        command.stderr(log);
        return command;
    }

    let script = if availability.is_some() {
        "exec 3<&0 4<&2 0</dev/null 2>&1; exec \"$0\" \"$@\""
    } else {
        "exec 3<&0 0</dev/null 2>&1; exec \"$0\" \"$@\""
    };
    let mut command = Command::new("sh");
    command.args(["-c", script]).arg(binary);
    if let Some(listener) = public {
        command
            .env(PUBLIC_LISTENER_FD_ENV, "3")
            .stdin(Stdio::from(OwnedFd::from(listener)));
    }
    if let Some(listener) = availability {
        command
            .env(AVAILABILITY_LISTENER_FD_ENV, "4")
            .stderr(Stdio::from(OwnedFd::from(listener)));
    } else {
        command.stderr(log);
    }
    command
}

#[cfg(not(unix))]
fn inherited_listener_command(binary: &Path, listeners: NodeListeners, log: std::fs::File) -> Command {
    drop(listeners);
    let mut command = Command::new(binary);
    command.stderr(log);
    command
}

fn peryx_binary() -> PathBuf {
    std::env::var_os("PERYX_BIN").map_or_else(|| PathBuf::from("peryx"), PathBuf::from)
}

/// Put a child in its own process group so the harness can signal all descendants.
fn spawn_in_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let _ = command;
}

/// Kill a child's entire process group and reap it, so no descendant leaks.
fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child leads its own group (spawned by `spawn_in_group`), so its pid names the group.
        let group = nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("pid fits an i32"));
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

struct ListenerReservation {
    port: u16,
    #[cfg(unix)]
    listener: Option<TcpListener>,
}

impl ListenerReservation {
    fn ephemeral() -> std::io::Result<Self> {
        Self::bind(0)
    }

    fn bind(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            port,
            #[cfg(unix)]
            listener: Some(listener),
        })
    }

    const fn released(port: u16) -> Self {
        Self {
            port,
            #[cfg(unix)]
            listener: None,
        }
    }
}

struct NodeListeners {
    public: ListenerReservation,
    availability: ListenerReservation,
}

impl NodeListeners {
    fn ephemeral(with_availability: bool) -> std::io::Result<Self> {
        Ok(Self {
            public: ListenerReservation::ephemeral()?,
            availability: if with_availability {
                ListenerReservation::ephemeral()?
            } else {
                ListenerReservation::released(0)
            },
        })
    }

    fn on_ports(public: u16, availability: u16) -> std::io::Result<Self> {
        Ok(Self {
            public: ListenerReservation::bind(public)?,
            availability: if availability == 0 {
                ListenerReservation::released(0)
            } else {
                ListenerReservation::bind(availability)?
            },
        })
    }

    const fn public(public: ListenerReservation) -> Self {
        Self {
            public,
            availability: ListenerReservation::released(0),
        }
    }

    const fn ports(&self) -> (u16, u16) {
        (self.public.port, self.availability.port)
    }
}

fn free_port() -> u16 {
    ListenerReservation::ephemeral().expect("bind ephemeral").port
}

/// Spawn a stand-alone node forced onto `port`, so a self-test can prove the harness detects a port
/// collision instead of hanging or attaching to a foreign server.
///
/// # Errors
/// The [`HarnessError`] the losing process reports while failing to come up.
pub fn spawn_on_port(identity: &str, port: u16) -> Result<Node, HarnessError> {
    ProcessHarness::default().spawn_on_port(identity, port)
}

/// Spawn a stand-alone node from a raw config, so a self-test can drive a startup failure.
///
/// # Errors
/// The [`HarnessError`] the process reports while failing to come up.
pub fn spawn_with_config(identity: &str, config_toml: &str) -> Result<Node, HarnessError> {
    ProcessHarness::default().spawn_with_config(identity, config_toml)
}

#[must_use]
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Signalling with `None` checks existence without delivering a signal.
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        let pid = nix::unistd::Pid::from_raw(pid);
        nix::sys::signal::kill(pid, None).is_ok()
    }
    #[cfg(windows)]
    {
        windows_process_alive(pid)
    }
}

#[cfg(windows)]
#[allow(unsafe_code, reason = "Windows exposes process status through raw handle APIs")]
fn windows_process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut status = 0;
        let active = GetExitCodeProcess(process, &raw mut status) != 0 && status == STILL_ACTIVE as u32;
        CloseHandle(process);
        active
    }
}

#[must_use]
pub fn reachable_through(endpoint: &str) -> bool {
    http_client(HTTP_TIMEOUT)
        .get(format!("http://{endpoint}/+status"))
        .send()
        .is_ok_and(|response| response.status().is_success())
}
