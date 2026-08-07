//! A multi-process availability test harness: spawn real `peryx serve` binaries with isolated stores
//! and a generated datacenter roster, observe them over their public HTTP surface, inject network
//! faults through Toxiproxy, and tear the whole group down without leaking a process.
//!
//! The harness drives production binaries and public APIs only. It never links a peryx crate to reach
//! into private state, so a test asserts through `/+status`, `/+ready`, and `/+availability/topology`
//! the way an operator would. Every spawned process (peryx nodes and `toxiproxy-server`) runs in its own
//! process group and is killed on [`Drop`], so a panicking test leaks nothing.
//!
//! The ownership consensus plane is only partly reachable today: the embedded Raft node ([#498]) runs,
//! but no write or authority endpoint is exposed over HTTP yet, and a multi-node group cannot form
//! because the inbound peer-RPC router is not mounted. So [`OwnershipControl`] is defined but its methods
//! return [`HarnessError::Unsupported`]; the failover test tier fills them once [#540] lands.

#![allow(
    dead_code,
    unused_imports,
    reason = "a reusable harness exposes surface the self-tests do not each exercise"
)]

pub mod toxiproxy;

use std::fmt::Write as _;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use peryx_storage::blob::Digest;
use tempfile::TempDir;

pub use toxiproxy::{Proxy, Toxiproxy};

const BIN: &str = env!("CARGO_BIN_EXE_peryx");
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(25);
// A dead node refuses the connection at once, so this bounds only how long a live-but-slow response
// may take. Under the parallel availability-e2e suite a loaded CI runner can push a first read past a
// two-second ceiling, which then reads as a spurious unreachable; keep it well under READY_TIMEOUT.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// The administrator a topology bootstraps when it opts in with [`Topology::with_admin`], and the
/// password it authenticates with. The password clears the fifteen-character floor
/// `bootstrap-administrator` enforces, so a test reads operator- and administrator-class fields with
/// [`Node::http_get_as`].
pub const ADMIN_USER: &str = "harness-admin";
pub const ADMIN_PASSWORD: &str = "harness-admin-secret";

/// The write-granting secret the harness configures on every node's `hosted` index, so a test can
/// publish a package through [`Node::publish`] with the `__token__` upload convention.
pub const UPLOAD_TOKEN: &str = "harness-upload-secret";

/// A real wheel with parseable metadata (project `veloxdemo`, version `1.0.0`), so [`Node::publish`]
/// stages an artifact the publish path admits. Its digest and filename address the download.
pub const WHEEL: &[u8] = include_bytes!("../../../../tests/frontend/fixtures/veloxdemo-1.0.0-py3-none-any.whl");
/// The project and version the fixture wheel carries.
pub const WHEEL_PROJECT: &str = "veloxdemo";
pub const WHEEL_VERSION: &str = "1.0.0";
pub const WHEEL_FILENAME: &str = "veloxdemo-1.0.0-py3-none-any.whl";

/// The content address of the fixture [`WHEEL`]: its lowercase-hex SHA-256. The upload's
/// `sha256_digest` field carries it, and it addresses the artifact in the download URL
/// `/hosted/files/{digest}/{filename}`, so a test names the same blob it published.
#[must_use]
pub fn wheel_digest() -> String {
    Digest::of(WHEEL).as_str().to_owned()
}

/// The route of the hosted OCI index [`Topology::with_oci`] adds. The distribution API mounts at the
/// root `/v2/`, carrying the index route as a prefix on the repository name, so a test addresses an
/// image `app` as `/v2/{OCI_ROUTE}/app/...` on the public port.
pub const OCI_ROUTE: &str = "oci";

/// The OCI image-manifest media type [`Node::oci_put_manifest`] declares and [`Node::oci_get_manifest`]
/// accepts.
pub const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// The `sha256:<hex>` OCI content digest of `bytes`, the form the distribution API addresses blobs and
/// manifests by. The bare-hex [`wheel_digest`] is the same value without the algorithm prefix.
#[must_use]
pub fn oci_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Digest::of(bytes).as_str())
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
    #[error("peryx rejected the generated config:\n{0}")]
    Config(String),
    #[error("this control is not available yet: {0}")]
    Unsupported(&'static str),
    #[error("authority did not leave {from:?} within {within:?}; it still reports {observed:?}")]
    NoTransfer {
        from: String,
        within: Duration,
        observed: Option<String>,
    },
}

impl HarnessError {
    /// Whether a node aborted startup because another process had already bound its port. Ports are
    /// drawn by binding `:0` and releasing, so a parallel test can claim a just-freed port before this
    /// node's child re-binds it; the child then dies with `EADDRINUSE`. A whole-cluster retry on fresh
    /// ports clears it, since the port is baked into every node's shared roster and cannot be swapped
    /// after the fact.
    fn is_port_collision(&self) -> bool {
        matches!(self, Self::ExitedEarly { log, .. } if log.contains("Address already in use"))
    }
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

impl Mode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Dc => "dc",
            Self::Ha => "ha",
        }
    }
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
    oci: bool,
    write_ack_deadline_secs: Option<u64>,
}

impl Topology {
    /// A single stand-alone `none`-mode node, the simplest thing the harness can run.
    #[must_use]
    pub fn single() -> Self {
        Self {
            mode: Mode::None,
            group: "solo".to_owned(),
            token: "harness-token".to_owned(),
            members: vec![MemberSpec::new("node-a", "local", Role::Writer)],
            bootstrap_admin: false,
            oci: false,
            write_ack_deadline_secs: None,
        }
    }

    /// An `ha` group over the given members, running the embedded ownership Raft node on each.
    #[must_use]
    pub fn ha(group: &str, members: Vec<MemberSpec>) -> Self {
        Self {
            mode: Mode::Ha,
            group: group.to_owned(),
            token: "harness-token".to_owned(),
            members,
            bootstrap_admin: false,
            oci: false,
            write_ack_deadline_secs: None,
        }
    }

    /// A `dc` group over the given members: one writer and its read replicas within a datacenter.
    #[must_use]
    pub fn dc(group: &str, members: Vec<MemberSpec>) -> Self {
        Self {
            mode: Mode::Dc,
            group: group.to_owned(),
            token: "harness-token".to_owned(),
            members,
            bootstrap_admin: false,
            oci: false,
            write_ack_deadline_secs: None,
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

    /// Add a hosted OCI index at route [`OCI_ROUTE`] to every node, alongside the `PyPI` `hosted` index,
    /// so a test drives the distribution-spec `/v2/` surface: a `GET /v2/` handshake, a blob push, and a
    /// manifest publish. Opt-in and additive - the `PyPI` publish helpers keep working unchanged, and a
    /// topology that does not call this serves no `/v2/` API at all.
    #[must_use]
    pub const fn with_oci(mut self) -> Self {
        self.oci = true;
        self
    }

    #[must_use]
    pub const fn with_write_ack_deadline(mut self, seconds: u64) -> Self {
        self.write_ack_deadline_secs = Some(seconds);
        self
    }

    /// Spawn every member and wait until each answers `/+status`.
    ///
    /// A member whose freed port a parallel test reclaimed before its child re-bound it aborts with
    /// `EADDRINUSE`; because the port is fixed in every node's shared roster it cannot be swapped in
    /// place, so the whole cluster is torn down and retried on a fresh draw. Independent draws colliding
    /// on every attempt is vanishingly unlikely, so a small bound removes the race without masking a
    /// genuine bind failure, which recurs on every attempt and surfaces on the last.
    ///
    /// # Errors
    /// Returns the first [`HarnessError`] a node reports while coming up.
    pub fn start(&self) -> Result<Cluster, HarnessError> {
        const ATTEMPTS: usize = 5;
        let mut attempt = 1;
        loop {
            match self.spawn_once() {
                Ok(cluster) => return Ok(cluster),
                Err(error) if attempt < ATTEMPTS && error.is_port_collision() => attempt += 1,
                Err(error) => return Err(error),
            }
        }
    }

    /// One attempt at spawning the cluster on a fresh port draw. A partial cluster whose later member
    /// fails is dropped here, so [`Node`]'s own `Drop` kills every process it already started.
    fn spawn_once(&self) -> Result<Cluster, HarnessError> {
        let addresses: Vec<(u16, u16)> = self.members.iter().map(|_| (free_port(), free_port())).collect();
        let roster = self.roster_toml(&addresses);
        let writer = (self.mode != Mode::None).then(|| self.writer(&addresses));
        let mut nodes = Vec::with_capacity(self.members.len());
        for (member, &(public, control)) in self.members.iter().zip(&addresses) {
            let node = Node::spawn(self, member, public, control, writer.as_ref(), None, &roster)?;
            nodes.push(node);
        }
        Ok(Cluster { nodes })
    }

    /// Spawn the cluster with each replica's link to the writer routed through its own Toxiproxy proxy,
    /// so a test can slow or cut one replica's metadata-and-beacon link in isolation and watch the
    /// writer's group readiness react. A replica dials the proxy as its upstream; the proxy forwards to
    /// the writer's public port, so both the follower sync and the frontier beacon share the faultable
    /// link. When `healthy` is false each proxy starts partitioned, so a replica comes up but cannot
    /// reach the writer until the test heals it - a replica that joins the group after the writer.
    ///
    /// Only meaningful for a `dc`/`ha` topology, the one shape with a writer replicas follow.
    ///
    /// # Errors
    /// The first [`HarnessError`] a node or the proxy server reports while coming up. Retries the whole
    /// draw on a port collision, exactly as [`start`](Self::start) does.
    pub fn start_proxied(&self, toxiproxy: &mut Toxiproxy, healthy: bool) -> Result<Proxied, HarnessError> {
        const ATTEMPTS: usize = 5;
        let mut attempt = 1;
        loop {
            match self.spawn_proxied(toxiproxy, healthy) {
                Ok(proxied) => return Ok(proxied),
                Err(error) if attempt < ATTEMPTS && error.is_port_collision() => attempt += 1,
                Err(error) => return Err(error),
            }
        }
    }

    fn spawn_proxied(&self, toxiproxy: &mut Toxiproxy, healthy: bool) -> Result<Proxied, HarnessError> {
        let addresses: Vec<(u16, u16)> = self.members.iter().map(|_| (free_port(), free_port())).collect();
        let roster = self.roster_toml(&addresses);
        let writer = self.writer(&addresses);
        let mut nodes = Vec::with_capacity(self.members.len());
        let mut proxies = std::collections::HashMap::new();
        for (member, &(public, control)) in self.members.iter().zip(&addresses) {
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
                public,
                control,
                Some(&writer),
                upstream.as_deref(),
                &roster,
            )?;
            nodes.push(node);
        }
        Ok(Proxied {
            cluster: Cluster { nodes },
            proxies,
        })
    }

    /// Validate the generated config for the first member through `peryx config check`, without spawning
    /// a server or forming a cluster. This proves the topology produces configuration peryx accepts,
    /// which is the reachable assertion while the ownership consensus plane is only partly wired.
    ///
    /// # Errors
    /// [`HarnessError::Config`] with the validator's output when peryx rejects the generated config.
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
        let dir = TempDir::new().expect("temp dir");
        let config = dir.path().join("peryx.toml");
        std::fs::write(
            &config,
            node_config(self, member, addresses[0].1, writer.as_ref(), None, &roster),
        )
        .expect("write config");
        let output = Command::new(BIN)
            .args(["config", "check"])
            .arg("--config")
            .arg(&config)
            .arg("--data-dir")
            .arg(dir.path())
            .output()
            .expect("run peryx config check");
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
        if self.mode == Mode::None {
            return String::new();
        }
        let mut toml = format!(
            "[availability]\nmode = \"{}\"\ngroup = \"{}\"\n\n",
            self.mode.as_str(),
            self.group,
        );
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

    /// The writer's identity and public address, which every replica follows and claims before it
    /// starts. A `dc` or `ha` topology always has exactly one writer.
    fn writer(&self, addresses: &[(u16, u16)]) -> (String, u16) {
        self.members
            .iter()
            .zip(addresses)
            .find(|(member, _)| matches!(member.role, Role::Writer))
            .map(|(member, &(public, _))| (member.node.clone(), public))
            .expect("a dc or ha topology has a writer")
    }
}

/// A cluster spawned with [`Topology::start_proxied`]: every replica reaches its writer through a
/// dedicated Toxiproxy proxy, so a test can pause or cut one replica's link to the writer in isolation.
/// Dropping it tears the cluster down; the proxies belong to the [`Toxiproxy`] the caller still holds.
pub struct Proxied {
    cluster: Cluster,
    proxies: std::collections::HashMap<String, Proxy>,
}

impl Proxied {
    /// The spawned cluster.
    #[must_use]
    pub const fn cluster(&self) -> &Cluster {
        &self.cluster
    }

    /// The spawned cluster, for mutation (kill, restart, wait).
    pub const fn cluster_mut(&mut self) -> &mut Cluster {
        &mut self.cluster
    }

    /// The proxy fronting a replica's link to the writer, by the replica's configured identity.
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
    /// The cluster's nodes, in roster order.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The cluster's nodes for mutation (kill, restart, wait).
    pub fn nodes_mut(&mut self) -> &mut [Node] {
        &mut self.nodes
    }

    /// A node by its configured identity.
    #[must_use]
    pub fn node(&self, identity: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.identity == identity)
    }

    /// A failure artifact for every node: topology, process status, recent log, and pending operations.
    #[must_use]
    pub fn failure_report(&self) -> FailureReport {
        FailureReport {
            nodes: self.nodes.iter().map(Node::snapshot).collect(),
        }
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
        let deadline = Instant::now() + within;
        loop {
            let observed = self.observed_leader();
            if let Some(leader) = &observed
                && leader != from
            {
                return Ok(leader.clone());
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::NoTransfer {
                    from: from.to_owned(),
                    within,
                    observed,
                });
            }
            std::thread::sleep(READY_POLL);
        }
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
}

impl Node {
    fn spawn(
        topology: &Topology,
        member: &MemberSpec,
        port: u16,
        control_port: u16,
        writer: Option<&(String, u16)>,
        upstream: Option<&str>,
        roster: &str,
    ) -> Result<Self, HarnessError> {
        let data = TempDir::new().expect("temp data dir");
        let config = data.path().join("peryx.toml");
        std::fs::write(
            &config,
            node_config(topology, member, control_port, writer, upstream, roster),
        )
        .expect("write config");
        if topology.bootstrap_admin {
            bootstrap_admin(&config, data.path());
        }
        // A replica starts read-only and only verifies the writer identity, so its store must already
        // hold it: seed it offline through the same binary before serving.
        if writer.is_some() && !matches!(member.role, Role::Writer) {
            claim_writer_identity(&config, data.path())?;
        }
        let http = reqwest::blocking::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("build http client");
        let mut node = Self {
            identity: member.node.clone(),
            child: launch(&config, data.path(), port),
            port,
            control_port,
            config,
            data,
            http,
        };
        node.await_ready()?;
        Ok(node)
    }

    /// Poll `/+status` until this node answers, exits, or the deadline passes.
    ///
    /// # Errors
    /// [`HarnessError::ExitedEarly`] when the child dies first, [`HarnessError::NotReady`] on timeout.
    pub fn await_ready(&mut self) -> Result<(), HarnessError> {
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("child status") {
                return Err(HarnessError::ExitedEarly {
                    node: self.identity.clone(),
                    status: status.to_string(),
                    log: self.log_tail(),
                });
            }
            if self.is_ready() {
                return Ok(());
            }
            std::thread::sleep(READY_POLL);
        }
        Err(HarnessError::NotReady {
            node: self.identity.clone(),
            timeout: READY_TIMEOUT,
            log: self.log_tail(),
        })
    }

    /// Spawn a bare `none`-mode node with an explicit port and raw config, for harness self-tests that
    /// force a port collision or an invalid configuration.
    fn start_raw(identity: &str, port: u16, config_toml: String) -> Result<Self, HarnessError> {
        let data = TempDir::new().expect("temp data dir");
        let config = data.path().join("peryx.toml");
        std::fs::write(&config, config_toml).expect("write config");
        let http = reqwest::blocking::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("build http client");
        let mut node = Self {
            identity: identity.to_owned(),
            child: launch(&config, data.path(), port),
            port,
            control_port: 0,
            config,
            data,
            http,
        };
        node.await_ready()?;
        Ok(node)
    }

    /// The node's public HTTP port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The node process's pid, for a leaked-process assertion.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The `host:port` peers dial for this node's control plane, the target a Toxiproxy proxy fronts.
    #[must_use]
    pub fn control_endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.control_port)
    }

    /// The node's configured identity.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Whether the process is still running (has not exited).
    #[must_use]
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Whether `/+status` answers `200` with a peryx body.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status()
            .is_some_and(|(code, body)| code == 200 && body.contains("\"version\""))
    }

    /// `GET /+status`, or `None` when the node is unreachable.
    #[must_use]
    pub fn status(&self) -> Option<(u16, String)> {
        self.http_get("/+status")
    }

    /// `GET /+ready`, or `None` when the node is unreachable.
    #[must_use]
    pub fn readiness(&self) -> Option<(u16, String)> {
        self.http_get("/+ready")
    }

    /// `GET /+availability/topology`, or `None` when the node is unreachable or serves no topology.
    #[must_use]
    pub fn topology(&self) -> Option<(u16, String)> {
        self.http_get("/+availability/topology")
    }

    /// `GET /metrics`, the Prometheus exposition, or `None` when the node is unreachable.
    #[must_use]
    pub fn metrics(&self) -> Option<(u16, String)> {
        self.http_get("/metrics")
    }

    /// `GET /+availability/placements`, the artifact placement view, or `None` when the node is
    /// unreachable or serves no placement view.
    #[must_use]
    pub fn placements(&self) -> Option<(u16, String)> {
        self.http_get("/+availability/placements")
    }

    /// `GET {path}` against the node's public port, returning the status code and body, or `None` when
    /// the node is unreachable. This is the general accessor the typed observations build on, so a test
    /// can reach any read endpoint without the harness naming it first.
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

    /// `GET {path}` with HTTP Basic credentials, returning the status code and body, or `None` when the
    /// node is unreachable. Pair it with a [`Topology::with_admin`] group to read the operator- and
    /// administrator-class fields the anonymous [`Self::http_get`] never sees.
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

    /// Publish the fixture [`WHEEL`] to this node's local `hosted` index through the legacy multipart
    /// upload API twine and `uv publish` drive, authenticating with the `__token__` convention and
    /// [`UPLOAD_TOKEN`]. Returns the upload's status and body (`200` and `upload accepted` on success),
    ///
    /// # Errors
    /// Returns a transport error when the request cannot complete.
    ///
    /// Publishing to one node, not another, is how a test places a blob on a single datacenter: the
    /// bytes land only where they are uploaded, so a sibling that never replicated them answers a local
    /// [`Self::download_wheel`] with `404` until the peer-serve read-through fills it.
    pub fn publish(&self) -> Result<(u16, String), reqwest::Error> {
        let content = reqwest::blocking::multipart::Part::bytes(WHEEL.to_vec()).file_name(WHEEL_FILENAME);
        let form = reqwest::blocking::multipart::Form::new()
            .text(":action", "file_upload")
            .text("name", WHEEL_PROJECT)
            .text("version", WHEEL_VERSION)
            .text("filetype", "bdist_wheel")
            .text("sha256_digest", wheel_digest())
            .part("content", content);
        let response = self
            .http
            .post(format!("http://127.0.0.1:{}/hosted/", self.port))
            .basic_auth("__token__", Some(UPLOAD_TOKEN))
            .multipart(form)
            .send()?;
        let code = response.status().as_u16();
        Ok((code, response.text().unwrap_or_default()))
    }

    /// `GET {path}` against the public port, returning the status and the raw response body, or `None`
    /// when the node is unreachable. The bytes counterpart to [`Self::http_get`], for an artifact whose
    /// body is binary and must survive a byte-for-byte comparison rather than a lossy UTF-8 decode.
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

    /// Download the fixture [`WHEEL`] from this node by its content address, the local file URL a Simple
    /// detail page points at: `GET /hosted/files/{sha256}/{filename}`. Returns the status and bytes, or
    /// `None` when unreachable. A node that holds the blob answers `200` with bytes equal to [`WHEEL`];
    /// one that does not answers `404`, which the peer-serve read-through turns into a remote fetch.
    #[must_use]
    pub fn download_wheel(&self) -> Option<(u16, Vec<u8>)> {
        self.download(&format!("/hosted/files/{}/{WHEEL_FILENAME}", wheel_digest()))
    }

    /// The OCI registry version check on the public port: `GET /v2/`, the distribution-spec handshake a
    /// client makes before any pull or push. `Some((200, _))` on a node serving an OCI index (a topology
    /// built with [`Topology::with_oci`]), `Some((404, _))` on one without, or `None` when unreachable.
    #[must_use]
    pub fn oci_v2(&self) -> Option<(u16, String)> {
        self.http_get("/v2/")
    }

    /// Push `blob` to `repo` under the hosted OCI index by a monolithic upload, authenticating with
    /// [`UPLOAD_TOKEN`]. Returns the commit status and the blob's `sha256:<hex>` digest (`201` on
    /// success). Content-addressed, so re-pushing identical bytes commits under the same digest.
    ///
    /// # Errors
    /// Returns a transport error when the request cannot complete.
    pub fn oci_push_blob(&self, repo: &str, blob: &[u8]) -> Result<(u16, String), reqwest::Error> {
        let digest = oci_digest(blob);
        let response = self
            .http
            .post(format!(
                "http://127.0.0.1:{}/v2/{OCI_ROUTE}/{repo}/blobs/uploads/?digest={digest}",
                self.port,
            ))
            .basic_auth("_", Some(UPLOAD_TOKEN))
            .body(blob.to_vec())
            .send()?;
        Ok((response.status().as_u16(), digest))
    }

    /// Pull the blob addressed by `digest` from `repo`: `GET /v2/{OCI_ROUTE}/{repo}/blobs/{digest}`.
    /// Returns the status and the raw bytes (`200` and the bytes on a hit, `404` on a miss), or `None`
    /// when unreachable.
    #[must_use]
    pub fn oci_pull_blob(&self, repo: &str, digest: &str) -> Option<(u16, Vec<u8>)> {
        self.download(&format!("/v2/{OCI_ROUTE}/{repo}/blobs/{digest}"))
    }

    /// Publish `manifest` to `repo` under `reference` (a tag or digest):
    /// `PUT /v2/{OCI_ROUTE}/{repo}/manifests/{reference}` with `media_type` and the upload credential.
    /// Returns the status and the manifest's own `sha256:<hex>` digest (`201` on success).
    ///
    /// # Errors
    /// Returns a transport error when the request cannot complete.
    pub fn oci_put_manifest(
        &self,
        repo: &str,
        reference: &str,
        manifest: &[u8],
        media_type: &str,
    ) -> Result<(u16, String), reqwest::Error> {
        let response = self
            .http
            .put(format!(
                "http://127.0.0.1:{}/v2/{OCI_ROUTE}/{repo}/manifests/{reference}",
                self.port,
            ))
            .basic_auth("_", Some(UPLOAD_TOKEN))
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .body(manifest.to_vec())
            .send()?;
        Ok((response.status().as_u16(), oci_digest(manifest)))
    }

    /// Resolve `reference` (a tag or digest) in `repo`: `GET /v2/{OCI_ROUTE}/{repo}/manifests/{reference}`.
    /// Returns the status, the `Docker-Content-Digest` the registry resolves it to, and the manifest
    /// bytes, or `None` when unreachable. A test names the tag it published and proves the tag still
    /// resolves to the same manifest digest across a failover.
    #[must_use]
    pub fn oci_get_manifest(&self, repo: &str, reference: &str) -> Option<(u16, Option<String>, Vec<u8>)> {
        let response = self
            .http
            .get(format!(
                "http://127.0.0.1:{}/v2/{OCI_ROUTE}/{repo}/manifests/{reference}",
                self.port,
            ))
            .header(reqwest::header::ACCEPT, OCI_MANIFEST_TYPE)
            .send()
            .ok()?;
        let code = response.status().as_u16();
        let digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Some((code, digest, response.bytes().ok()?.to_vec()))
    }

    /// The tags `repo` lists under the hosted OCI index: `GET /v2/{OCI_ROUTE}/{repo}/tags/list`, parsed to
    /// the `tags` array. Empty when the node is unreachable or lists none, so a test proves an idempotent
    /// retry left exactly one tag rather than a duplicate.
    #[must_use]
    pub fn oci_tags(&self, repo: &str) -> Vec<String> {
        let Some((200, body)) = self.http_get(&format!("/v2/{OCI_ROUTE}/{repo}/tags/list")) else {
            return Vec::new();
        };
        serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("tags")
                    .and_then(serde_json::Value::as_array)
                    .map(|tags| tags.iter().filter_map(|tag| tag.as_str().map(str::to_owned)).collect())
            })
            .unwrap_or_default()
    }

    /// Send a raw OCI mutation (`method` `path`) with the upload credential to the public port, for a verb
    /// the typed helpers do not name (a blob or manifest `DELETE`) or to observe the body a rejection
    /// carries. Returns the status and body, or `None` when unreachable. A read-only replica rejects it by
    /// method before routing reaches the OCI driver, so the credential only matters where a writer honors it.
    #[must_use]
    pub fn oci_mutate(&self, method: reqwest::Method, path: &str) -> Option<(u16, String)> {
        let response = self
            .http
            .request(method, format!("http://127.0.0.1:{}{path}", self.port))
            .basic_auth("_", Some(UPLOAD_TOKEN))
            .send()
            .ok()?;
        let code = response.status().as_u16();
        Some((code, response.text().unwrap_or_default()))
    }

    /// `GET {path}` against this node's private availability control listener with admin Basic
    /// credentials, returning the status code and body, or `None` when unreachable.
    ///
    /// The ownership group's consensus status the node reports under `/availability/v1/status` lives on
    /// this listener, behind the administrator gate, so a test reads it here rather than the public port.
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

    /// Kill the node's process group, so a test can drive a crash or a partition-by-death.
    pub fn kill(&mut self) {
        kill_group(&mut self.child);
    }

    /// Kill and re-spawn the node against the same data directory and port, then wait until it is ready.
    ///
    /// # Errors
    /// The [`HarnessError`] the fresh process reports while coming up.
    pub fn restart(&mut self) -> Result<(), HarnessError> {
        self.kill();
        self.child = launch(&self.config, self.data.path(), self.port);
        self.await_ready()
    }

    /// The last of the node's own log, for failure diagnostics.
    #[must_use]
    pub fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(self.data.path().join("peryx.log")).unwrap_or_default();
        log.lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn snapshot(&self) -> NodeArtifact {
        NodeArtifact {
            identity: self.identity.clone(),
            topology: self.topology().map(|(_, body)| body),
            status: self.status().map(|(_, body)| body),
            log_tail: self.log_tail(),
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        kill_group(&mut self.child);
    }
}

/// A diagnostic bundle for a failed test: one entry per node.
#[derive(Debug)]
pub struct FailureReport {
    pub nodes: Vec<NodeArtifact>,
}

impl FailureReport {
    /// Render the report as text a failing assertion can print.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for node in &self.nodes {
            let _ = write!(
                out,
                "== node {} ==\ntopology: {}\nstatus: {}\nlog:\n{}\n\n",
                node.identity,
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
    pub topology: Option<String>,
    pub status: Option<String>,
    pub log_tail: String,
}

/// The ownership-plane controls the failover test tier drives a cluster through.
///
/// The control plane exposes its leader over the availability status resource, so [`leader`](Self::leader)
/// and [`await_authority_transfer`](Self::await_authority_transfer) observe a real control-plane failover:
/// kill the datacenter holding authority and watch it move. Submitting an ownership command still needs a
/// write endpoint that does not exist yet, so [`submit_ownership_write`](Self::submit_ownership_write)
/// stays blocked on #540 rather than faking a result.
pub trait OwnershipControl {
    /// Submit an ownership command to the current leader.
    ///
    /// # Errors
    /// [`HarnessError::Unsupported`] until an ownership write endpoint exists (#540).
    fn submit_ownership_write(&self, _command: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("ownership write endpoint is blocked on #540"))
    }

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

/// Generate one node's full config: a minimal hosted index every node serves, plus the availability and
/// roster blocks for a `dc` or `ha` member.
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
        // Every node follows the one writer's identity: the writer claims it on startup, and a replica
        // verifies its offline-seeded store against it. Consensus, though, needs each node's OWN identity,
        // so it names its own member entry through `node_identity` - otherwise every node would run the
        // ownership Raft node under the writer's voter id and the group could never fail over.
        let _ = writeln!(config, "writer_identity = \"{}\"", writer.0);
        let _ = writeln!(config, "node_identity = \"{}\"\n", member.node);
    }
    config.push_str("[[index]]\nname = \"hosted\"\nhosted = true\nvolatile = true\n\n");
    // A write-granting token so the publish helpers can upload; anonymous reads stay open for downloads.
    let _ = write!(
        config,
        "[[index.access_token]]\nname = \"uploader\"\nsecret = \"{UPLOAD_TOKEN}\"\nprojects = [\"*\"]\nactions = [\"write\", \"delete\"]\n\n",
    );
    if topology.oci {
        // A hosted OCI index at OCI_ROUTE, so the node answers the distribution-spec `/v2/` API and a test
        // can push a blob and a manifest. Its own credential mirrors the PyPI index's: a `[[index.access_token]]`
        // binds to the `[[index]]` it follows, so each hosted index carries its own upload token.
        let _ = write!(
            config,
            "[[index]]\nname = \"{OCI_ROUTE}\"\necosystem = \"oci\"\nhosted = true\nvolatile = true\n\n",
        );
        let _ = write!(
            config,
            "[[index.access_token]]\nname = \"oci-uploader\"\nsecret = \"{UPLOAD_TOKEN}\"\nprojects = [\"*\"]\nactions = [\"write\", \"delete\"]\n\n",
        );
    }
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

/// Create the first administrator in `data` before the node serves, through `peryx
/// bootstrap-administrator`, which writes the user offline while nothing holds the store. The node
/// then authenticates the same credential over HTTP Basic.
fn bootstrap_admin(config: &std::path::Path, data: &std::path::Path) {
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

/// Seed a replica's store with the writer identity it follows, through the same offline command an
/// operator runs, so the replica passes its read-only startup identity check before it serves.
fn claim_writer_identity(config: &std::path::Path, data: &std::path::Path) -> Result<(), HarnessError> {
    let output = Command::new(BIN)
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

fn launch(config: &std::path::Path, data: &std::path::Path, port: u16) -> Child {
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
        .args(["--log-level", "info"])
        .stdout(log.try_clone().expect("clone log handle"))
        .stderr(log);
    spawn_in_group(&mut command);
    command.spawn().expect("spawn peryx")
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

/// Grab a free loopback port by binding `:0` and releasing it. A spawned process re-binds it a moment
/// later; each node uses a distinct port so parallel runs stay separate.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Spawn a stand-alone node forced onto `port`, so a self-test can prove the harness detects a port
/// collision instead of hanging or attaching to a foreign server.
///
/// # Errors
/// The [`HarnessError`] the losing process reports while failing to come up.
pub fn spawn_on_port(identity: &str, port: u16) -> Result<Node, HarnessError> {
    Node::start_raw(
        identity,
        port,
        "[[index]]\nname = \"hosted\"\nhosted = true\n".to_owned(),
    )
}

/// Spawn a stand-alone node from a raw config, so a self-test can drive a startup failure.
///
/// # Errors
/// The [`HarnessError`] the process reports while failing to come up.
pub fn spawn_with_config(identity: &str, config_toml: &str) -> Result<Node, HarnessError> {
    Node::start_raw(identity, free_port(), config_toml.to_owned())
}

/// Whether a process with `pid` still exists, for a leaked-process assertion.
#[must_use]
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Signalling with `None` performs the existence and permission check without delivering one.
        let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).expect("pid fits an i32"));
        nix::sys::signal::kill(pid, None).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Whether a peryx node answers `/+status` through a `host:port` endpoint (a Toxiproxy listen address).
#[must_use]
pub fn reachable_through(endpoint: &str) -> bool {
    reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("build http client")
        .get(format!("http://{endpoint}/+status"))
        .send()
        .is_ok_and(|response| response.status().is_success())
}
