//! A deterministic, offline simulator for the availability safety invariants.
//!
//! Wall-clock schedules make a partition-and-failover bug reproduce once and then vanish. This
//! module replaces the clock with a seed: a [`Config`] fixes the topology, step count, and an
//! optional injected [`Defect`], [`run`] expands that seed into a fixed [`Action`] plan, and
//! [`execute`] drives the plan through an explicit model that owns its storage, network, and
//! authority state. Nothing here touches the network, the filesystem, or a real clock, so one seed
//! yields one [`Trace`] on every platform.
//!
//! The model checks the five [`Invariant`]s the [availability contract](https://peryx.dev) makes
//! normative: one authority epoch per committed serial, idempotent apply, monotonic frontiers,
//! visibility that never regresses after commit, and no loss of an acknowledged operation. Each
//! [`Defect`] disables exactly one safety rule, so a passing checker is one that turns the matching
//! injected defect into the matching invariant failure. A failing seed reduces to a minimal action
//! plan through [`minimize`], and the whole run serializes into one [`Trace`] artifact holding the
//! seed, topology, faults, actions, and the first invariant failure.
//!
//! This is a model of the replica contract, not a driver of the production replica. It carries its
//! own explicit state rather than calling [`Replica::sync_once`](crate::Replica::sync_once), and it
//! delivers each authority's stream in serial order, so it exercises fencing, replay, and lag but
//! not message reorder or the wire-level page validation [`Replica`](crate::Replica) already tests.
//! Driving the production replica from a seeded plan would extend the harness to catch a regression
//! in `sync_once` itself.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::envelope::{AuthorityEpoch, OperationKind};

const KINDS: [OperationKind; 6] = [
    OperationKind::Publish,
    OperationKind::Withdraw,
    OperationKind::Delete,
    OperationKind::CacheFill,
    OperationKind::Publish,
    OperationKind::Delete,
];

const ACTION_SHAPES: NonZeroUsize = NonZeroUsize::new(6).unwrap();
const KIND_CHOICES: NonZeroUsize = NonZeroUsize::new(KINDS.len()).unwrap();

/// A reproducible splitmix64 stream: the only source of nondeterminism the simulator admits.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Start the stream at `seed`.
    #[must_use]
    pub const fn seeded(seed: u64) -> Self {
        Self { state: seed }
    }

    const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: NonZeroUsize) -> usize {
        usize::try_from(self.next_u64() % bound.get() as u64).unwrap_or(0)
    }
}

/// The static shape of the modelled cluster: how many replicating nodes observe how many authorities.
///
/// Both counts are [`NonZeroUsize`], so a zero-dimension cluster cannot be built or deserialized and
/// the seeded index draws below never divide by zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub nodes: NonZeroUsize,
    pub sources: NonZeroUsize,
}

impl Topology {
    /// Build a topology, rejecting a zero node or source count.
    #[must_use]
    pub fn new(nodes: usize, sources: usize) -> Option<Self> {
        Some(Self {
            nodes: NonZeroUsize::new(nodes)?,
            sources: NonZeroUsize::new(sources)?,
        })
    }
}

/// A single disabled safety rule, injected to prove the matching [`Invariant`] checker can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Defect {
    /// Commit an operation whose epoch the authority already superseded.
    AcceptStaleEpoch,
    /// Apply an already-committed operation a second time instead of ignoring it.
    ReapplyDuplicate,
    /// Rewind a node's frontier below a serial it already committed.
    RegressFrontier,
    /// Drop an operation a node had already made visible.
    HideCommitted,
    /// Acknowledge an operation the authority never made durable.
    LoseAcknowledged,
}

/// A safety property the model checks after every action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Invariant {
    /// Every committed serial carries exactly the authority's decision for it.
    Authority,
    /// Re-delivering an operation changes nothing.
    Idempotency,
    /// A node's frontier never decreases.
    Frontier,
    /// An operation stays visible once a node commits it.
    Visibility,
    /// Every acknowledged operation is durable at the authority.
    Rpo,
}

/// One scheduled step. A plan is a `Vec<Action>`; the same plan always produces the same [`Trace`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Action {
    /// The current authority for `source` commits its next serial.
    Produce { source: usize, kind: OperationKind },
    /// A new authority generation takes over `source`, fencing the previous one.
    Failover { source: usize },
    /// A fenced former authority for `source` emits an operation at its stale epoch.
    StaleProduce { source: usize },
    /// Deliver the next queued operation to `node`.
    Deliver { node: usize },
    /// Re-observe the operation `node` last committed for `source`.
    Redeliver { node: usize, source: usize },
    /// Acknowledge every durable operation for `source` back to its client.
    Acknowledge { source: usize },
}

/// A fault the run injected, recorded alongside the actions that carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fault", rename_all = "kebab-case")]
pub enum Fault {
    /// The configured safety-rule defect the whole run carried.
    Injected { defect: Defect },
    /// An authority handover for `source` at plan step `step`.
    Failover { source: usize, step: usize },
    /// A fenced former authority for `source` emitted at plan step `step`.
    StalePrimary { source: usize, step: usize },
}

/// Whether the run held every invariant or the first one it broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum Outcome {
    /// No invariant failed across the whole plan.
    Held,
    /// `invariant` failed after the action at `step`.
    Violated { invariant: Invariant, step: usize },
}

/// The single artifact a run records: seed, topology, faults, executed actions, and outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trace {
    pub seed: u64,
    pub topology: Topology,
    pub defect: Option<Defect>,
    pub faults: Vec<Fault>,
    pub actions: Vec<Action>,
    pub outcome: Outcome,
}

/// A seed and the shape of the run it expands into.
pub struct Config {
    pub seed: u64,
    pub topology: Topology,
    pub steps: usize,
    pub defect: Option<Defect>,
}

/// Expand `config`'s seed into a fixed action plan without running it.
#[must_use]
pub fn generate_plan(config: &Config) -> Vec<Action> {
    let mut rng = Rng::seeded(config.seed);
    let mut plan = Vec::with_capacity(config.steps);
    for _ in 0..config.steps {
        let node = rng.below(config.topology.nodes);
        let source = rng.below(config.topology.sources);
        plan.push(match rng.below(ACTION_SHAPES) {
            0 => Action::Produce {
                source,
                kind: KINDS[rng.below(KIND_CHOICES)],
            },
            1 => Action::Failover { source },
            2 => Action::StaleProduce { source },
            3 => Action::Deliver { node },
            4 => Action::Redeliver { node, source },
            _ => Action::Acknowledge { source },
        });
    }
    plan
}

/// Run a fixed `plan` against the model and return its trace.
#[must_use]
pub fn execute(config: &Config, plan: &[Action]) -> Trace {
    let mut world = World::new(config);
    let mut actions = Vec::new();
    let mut outcome = Outcome::Held;
    for (step, action) in plan.iter().enumerate() {
        world.step(step, action);
        actions.push(action.clone());
        if let Some(invariant) = world.check() {
            outcome = Outcome::Violated { invariant, step };
            break;
        }
    }
    Trace {
        seed: config.seed,
        topology: config.topology,
        defect: config.defect,
        faults: world.faults,
        actions,
        outcome,
    }
}

/// Expand and run `config` in one call.
#[must_use]
pub fn run(config: &Config) -> Trace {
    execute(config, &generate_plan(config))
}

/// Greedily shrink `plan` to a minimal prefix-free subsequence that still breaks the same invariant.
///
/// A plan that holds every invariant is returned unchanged.
#[must_use]
pub fn minimize(config: &Config, plan: &[Action]) -> Vec<Action> {
    let Outcome::Violated { invariant, .. } = execute(config, plan).outcome else {
        return plan.to_vec();
    };
    let mut current = plan.to_vec();
    let mut changed = true;
    while changed {
        changed = false;
        let mut index = 0;
        while index < current.len() {
            let mut candidate = current.clone();
            candidate.remove(index);
            if still_breaks(config, &candidate, invariant) {
                current = candidate;
                changed = true;
            } else {
                index += 1;
            }
        }
    }
    current
}

fn still_breaks(config: &Config, plan: &[Action], invariant: Invariant) -> bool {
    matches!(
        execute(config, plan).outcome,
        Outcome::Violated { invariant: broken, .. } if broken == invariant
    )
}

#[derive(Clone, PartialEq, Eq)]
struct Committed {
    epoch: AuthorityEpoch,
    kind: OperationKind,
}

struct Message {
    source: usize,
    serial: u64,
    epoch: AuthorityEpoch,
    kind: OperationKind,
}

struct Node {
    frontier: Vec<u64>,
    accepted: Vec<AuthorityEpoch>,
    log: BTreeMap<(usize, u64), Committed>,
    applied: BTreeMap<(usize, u64), u32>,
    ever_visible: BTreeSet<(usize, u64)>,
    queue: VecDeque<Message>,
}

struct World {
    sources: usize,
    defect: Option<Defect>,
    epoch: Vec<AuthorityEpoch>,
    next_serial: Vec<u64>,
    truth: BTreeMap<(usize, u64), Committed>,
    acked: BTreeSet<(usize, u64)>,
    nodes: Vec<Node>,
    high: Vec<Vec<u64>>,
    faults: Vec<Fault>,
}

impl World {
    fn new(config: &Config) -> Self {
        let nodes = config.topology.nodes.get();
        let sources = config.topology.sources.get();
        let node = || Node {
            frontier: vec![0; sources],
            accepted: vec![AuthorityEpoch(0); sources],
            log: BTreeMap::new(),
            applied: BTreeMap::new(),
            ever_visible: BTreeSet::new(),
            queue: VecDeque::new(),
        };
        let mut faults = Vec::new();
        if let Some(defect) = config.defect {
            faults.push(Fault::Injected { defect });
        }
        Self {
            sources,
            defect: config.defect,
            epoch: vec![AuthorityEpoch(1); sources],
            next_serial: vec![0; sources],
            truth: BTreeMap::new(),
            acked: BTreeSet::new(),
            nodes: (0..nodes).map(|_| node()).collect(),
            high: vec![vec![0; sources]; nodes],
            faults,
        }
    }

    fn step(&mut self, step: usize, action: &Action) {
        match *action {
            Action::Produce { source, kind } => self.produce(source, kind),
            Action::Failover { source } => self.failover(source, step),
            Action::StaleProduce { source } => self.stale_produce(source, step),
            Action::Deliver { node } => self.deliver(node),
            Action::Redeliver { node, source } => self.redeliver(node, source),
            Action::Acknowledge { source } => self.acknowledge(source),
        }
    }

    fn produce(&mut self, source: usize, kind: OperationKind) {
        let epoch = self.epoch[source];
        let serial = self.next_serial[source] + 1;
        self.next_serial[source] = serial;
        self.truth.insert((source, serial), Committed { epoch, kind });
        for node in &mut self.nodes {
            node.queue.push_back(Message {
                source,
                serial,
                epoch,
                kind,
            });
        }
    }

    fn failover(&mut self, source: usize, step: usize) {
        self.epoch[source] = AuthorityEpoch(self.epoch[source].0 + 1);
        self.faults.push(Fault::Failover { source, step });
    }

    fn stale_produce(&mut self, source: usize, step: usize) {
        let epoch = AuthorityEpoch(self.epoch[source].0.saturating_sub(1));
        let serial = self.next_serial[source] + 1;
        for node in &mut self.nodes {
            node.queue.push_back(Message {
                source,
                serial,
                epoch,
                kind: KINDS[0],
            });
        }
        self.faults.push(Fault::StalePrimary { source, step });
    }

    fn deliver(&mut self, node: usize) {
        if let Some(message) = self.nodes[node].queue.pop_front() {
            self.apply(node, &message);
        }
    }

    fn apply(&mut self, node: usize, message: &Message) {
        let source = message.source;
        let authorized = self
            .truth
            .get(&(source, message.serial))
            .is_some_and(|committed| committed.epoch == message.epoch && committed.kind == message.kind);
        if !authorized && self.defect != Some(Defect::AcceptStaleEpoch) {
            return;
        }
        let committed = self.truth.get(&(source, message.serial)).cloned().unwrap_or(Committed {
            epoch: message.epoch,
            kind: message.kind,
        });
        let target = &mut self.nodes[node];
        target.frontier[source] = message.serial;
        if message.epoch > target.accepted[source] {
            target.accepted[source] = message.epoch;
        }
        target.log.insert((source, message.serial), committed);
        *target.applied.entry((source, message.serial)).or_insert(0) += 1;
        target.ever_visible.insert((source, message.serial));
    }

    fn redeliver(&mut self, node: usize, source: usize) {
        let serial = self.nodes[node].frontier[source];
        if serial == 0 {
            return;
        }
        let target = &mut self.nodes[node];
        match self.defect {
            Some(Defect::ReapplyDuplicate) => *target.applied.entry((source, serial)).or_insert(0) += 1,
            Some(Defect::RegressFrontier) => target.frontier[source] = serial - 1,
            Some(Defect::HideCommitted) => {
                target.log.remove(&(source, serial));
            }
            _ => {}
        }
    }

    fn acknowledge(&mut self, source: usize) {
        for serial in 1..=self.next_serial[source] {
            self.acked.insert((source, serial));
        }
        if self.defect == Some(Defect::LoseAcknowledged) {
            self.acked.insert((source, self.next_serial[source] + 1));
        }
    }

    fn check(&mut self) -> Option<Invariant> {
        for node in &self.nodes {
            for (key, committed) in &node.log {
                if self.truth.get(key) != Some(committed) {
                    return Some(Invariant::Authority);
                }
            }
        }
        for node in &self.nodes {
            if node.applied.values().any(|&count| count > 1) {
                return Some(Invariant::Idempotency);
            }
        }
        for node in 0..self.nodes.len() {
            for source in 0..self.sources {
                if self.nodes[node].frontier[source] < self.high[node][source] {
                    return Some(Invariant::Frontier);
                }
                self.high[node][source] = self.nodes[node].frontier[source];
            }
        }
        for node in &self.nodes {
            if node.ever_visible.iter().any(|key| !node.log.contains_key(key)) {
                return Some(Invariant::Visibility);
            }
        }
        for key in &self.acked {
            if !self.truth.contains_key(key) {
                return Some(Invariant::Rpo);
            }
        }
        None
    }
}

#[cfg(test)]
#[path = "../tests/unit/sim_tests.rs"]
mod sim_tests;
