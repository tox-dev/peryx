//! The availability safety-simulator seed campaign.
//!
//! `peryx_ha_distributed::sim` expands a `u64` seed into a fixed plan of authority handovers, deliveries,
//! and acknowledgements, runs it against a pure model of the replication plane, and checks five safety
//! invariants after every step. A correct model holds every invariant for every seed, so a violation on
//! any defect-free seed is a real regression. This runner sweeps a seed range across a topology matrix
//! looking for one, and on a break shrinks the plan with `minimize`, writes the trace as a JSON
//! artifact, and prints a one-line command that replays exactly that seed.
//!
//! It is deterministic and does no I/O beyond writing an artifact, so a seed reproduces identically on
//! any machine. The pull-request lane runs a bounded sweep plus a checked-in regression corpus within a
//! wall-time budget; a scheduled lane runs a large range split across matrix shards, each seed once. A
//! seed a scheduled shard turns up is promoted into `tests/corpus/availability-seeds.txt` so the
//! pull-request lane guards it forever.
//!
//! Gated behind `sim-campaign` so the default test run and the coverage gate skip it.

#![cfg(feature = "sim-campaign")]

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use peryx_ha_distributed::sim::{Config, Defect, Outcome, Topology, Trace, execute, generate_plan, minimize};

/// The cluster shapes every seed is run against when no single topology is pinned.
const MATRIX: &[(usize, usize)] = &[(2, 1), (3, 2), (4, 3), (5, 2)];

/// The bounded pull-request sweep; a scheduled shard overrides it with `PERYX_SIM_SEEDS`.
const DEFAULT_SEEDS: u64 = 512;
const DEFAULT_STEPS: usize = 200;
/// Cap the failures a single run reports, so a systemic break does not flood the log or artifact dir.
const MAX_REPORTED: usize = 10;

#[test]
fn test_availability_seed_campaign() {
    let plan = CampaignPlan::from_env();
    let corpus = load_corpus();
    let failures = plan.run(&corpus);
    assert!(failures.is_empty(), "{}", render_failures(&plan, &failures));
}

#[test]
fn test_an_injected_defect_produces_a_reportable_violation() {
    // Disabling a safety rule must make some seed break the matching invariant; the report path then
    // shrinks and serializes that trace. This exercises the failure path the defect-free sweep relies on
    // but, being correct, never takes.
    let topology = topology(3, 2);
    let violating = (0..256)
        .map(|seed| Config {
            seed,
            topology,
            steps: DEFAULT_STEPS,
            defect: Some(Defect::ReapplyDuplicate),
        })
        .find(|config| {
            matches!(
                execute(config, &generate_plan(config)).outcome,
                Outcome::Violated { .. }
            )
        })
        .expect("a disabled idempotency rule breaks the Idempotency invariant on some seed");

    let minimal = execute(&violating, &minimize(&violating, &generate_plan(&violating)));

    assert!(
        matches!(minimal.outcome, Outcome::Violated { .. }),
        "the minimized plan still violates"
    );
    assert!(
        serde_json::to_string(&minimal)
            .expect("the trace serializes")
            .contains("\"reapply-duplicate\""),
        "the serialized trace records the injected defect",
    );
}

#[test]
fn test_the_report_path_records_and_renders_a_violation() {
    // Drive the runner with a disabled safety rule so the sweep actually finds breaks, exercising the
    // record-and-render path the defect-free campaign never reaches.
    let artifacts = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sim-campaign-report");
    let plan = CampaignPlan {
        start: 0,
        seeds: 256,
        steps: DEFAULT_STEPS,
        shard_index: 0,
        shard_total: 1,
        topologies: vec![topology(3, 2)],
        artifacts,
        revision: "test".to_owned(),
        defect: Some(Defect::ReapplyDuplicate),
    };

    let failures = plan.run(&[]);

    assert!(
        !failures.is_empty(),
        "a disabled idempotency rule must surface a violation"
    );
    assert!(
        failures[0].artifact.exists(),
        "the failing trace is written as an artifact"
    );
    let report = render_failures(&plan, &failures);
    assert!(report.contains("repro: PERYX_SIM_START="), "{report}");
    assert!(report.contains("PERYX_SIM_TOPOLOGY=3x2"), "{report}");
}

#[test]
fn test_defect_names_parse_through_their_kebab_encoding() {
    assert_eq!(parse_defect("reapply-duplicate"), Some(Defect::ReapplyDuplicate));
    assert_eq!(parse_defect("regress-frontier"), Some(Defect::RegressFrontier));
    assert_eq!(parse_defect("not-a-defect"), None);
}

/// The seed range, topology set, and step count one invocation covers, resolved from the environment so
/// a CI shard and a local repro drive the same runner.
struct CampaignPlan {
    start: u64,
    seeds: u64,
    steps: usize,
    shard_index: u64,
    shard_total: u64,
    topologies: Vec<Topology>,
    artifacts: PathBuf,
    revision: String,
    /// A safety rule to disable for every run, so a fault-injection lane can prove the campaign reports a
    /// break instead of only trusting that it would. `None` is the real regression sweep.
    defect: Option<Defect>,
}

impl CampaignPlan {
    fn from_env() -> Self {
        let topologies = env::var("PERYX_SIM_TOPOLOGY").ok().map_or_else(
            || {
                MATRIX
                    .iter()
                    .map(|&(nodes, sources)| topology(nodes, sources))
                    .collect()
            },
            |pinned| vec![parse_topology(&pinned)],
        );
        Self {
            start: env_u64("PERYX_SIM_START", 0),
            seeds: env_u64("PERYX_SIM_SEEDS", DEFAULT_SEEDS),
            steps: usize::try_from(env_u64("PERYX_SIM_STEPS", DEFAULT_STEPS as u64)).unwrap_or(DEFAULT_STEPS),
            shard_index: env_u64("PERYX_SIM_SHARD_INDEX", 0),
            shard_total: env_u64("PERYX_SIM_SHARD_TOTAL", 1).max(1),
            topologies,
            artifacts: env::var("PERYX_SIM_ARTIFACTS").map_or_else(
                |_| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sim-campaign"),
                PathBuf::from,
            ),
            revision: env::var("GITHUB_SHA").unwrap_or_else(|_| "local".to_owned()),
            defect: env::var("PERYX_SIM_DEFECT").ok().and_then(|name| parse_defect(&name)),
        }
    }

    /// Every seed this invocation runs: the whole corpus (always, on every shard) plus the shard's slice
    /// of the sweep range.
    fn seeds(&self, corpus: &[u64]) -> Vec<u64> {
        let mut seeds: Vec<u64> = corpus.to_vec();
        seeds.extend(
            (0..self.seeds)
                .map(|offset| self.start.wrapping_add(offset))
                .filter(|seed| seed % self.shard_total == self.shard_index),
        );
        seeds
    }

    fn run(&self, corpus: &[u64]) -> Vec<Failure> {
        let mut failures = Vec::new();
        for &topology in &self.topologies {
            for &seed in &self.seeds(corpus) {
                let config = Config {
                    seed,
                    topology,
                    steps: self.steps,
                    defect: self.defect,
                };
                if let Outcome::Violated { .. } = execute(&config, &generate_plan(&config)).outcome {
                    failures.push(self.record(&config));
                    if failures.len() >= MAX_REPORTED {
                        return failures;
                    }
                }
            }
        }
        failures
    }

    /// Shrink a failing seed's plan and persist the minimal trace, so a red run ships the smallest
    /// counterexample rather than a full plan.
    fn record(&self, config: &Config) -> Failure {
        let minimal = execute(config, &minimize(config, &generate_plan(config)));
        fs::create_dir_all(&self.artifacts).expect("create the artifact directory");
        let path = self.artifacts.join(format!(
            "seed-{}-{}x{}.json",
            config.seed, config.topology.nodes, config.topology.sources
        ));
        fs::write(&path, serde_json::to_vec_pretty(&minimal).expect("serialize the trace")).expect("write the trace");
        Failure {
            seed: config.seed,
            topology: config.topology,
            trace: minimal,
            artifact: path,
        }
    }
}

/// One seed that broke an invariant, with its minimized trace and the artifact it was written to.
struct Failure {
    seed: u64,
    topology: Topology,
    trace: Trace,
    artifact: PathBuf,
}

fn render_failures(plan: &CampaignPlan, failures: &[Failure]) -> String {
    let mut out = format!(
        "availability seed campaign found {} invariant break(s)\nrevision {} shard {}/{}\n\n",
        failures.len(),
        plan.revision,
        plan.shard_index,
        plan.shard_total,
    );
    for failure in failures {
        let seed = failure.seed;
        let (nodes, sources) = (failure.topology.nodes, failure.topology.sources);
        let Outcome::Violated { invariant, step } = failure.trace.outcome else {
            continue;
        };
        let actions = failure.trace.actions.len();
        let artifact = failure.artifact.display();
        let steps = plan.steps;
        let _ = write!(
            out,
            "seed {seed} topology {nodes}x{sources}: {invariant:?} broke at step {step} ({actions} minimized actions)\n  \
             trace: {artifact}\n  \
             repro: PERYX_SIM_START={seed} PERYX_SIM_SEEDS=1 PERYX_SIM_SHARD_TOTAL=1 PERYX_SIM_TOPOLOGY={nodes}x{sources} PERYX_SIM_STEPS={steps} cargo test -p peryx --features sim-campaign --test sim_campaign\n\n",
        );
    }
    out
}

/// Read the always-run regression corpus: one seed per line, `#` comments and blanks ignored.
fn load_corpus() -> Vec<u64> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/availability-seeds.txt");
    let text = fs::read_to_string(&path).unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            line.parse()
                .unwrap_or_else(|_| panic!("corpus seed {line:?} is not a u64"))
        })
        .collect()
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn parse_topology(spec: &str) -> Topology {
    let (nodes, sources) = spec
        .split_once('x')
        .expect("PERYX_SIM_TOPOLOGY is `<nodes>x<sources>`, e.g. 3x2");
    topology(
        nodes.parse().expect("topology node count"),
        sources.parse().expect("topology source count"),
    )
}

fn topology(nodes: usize, sources: usize) -> Topology {
    Topology::new(nodes, sources).expect("a topology needs a non-zero node and source count")
}

/// Parse a `PERYX_SIM_DEFECT` name through the defect's own kebab-case encoding, so the fault-injection
/// lane names a defect the way the trace records it.
fn parse_defect(name: &str) -> Option<Defect> {
    serde_json::from_str(&format!("\"{name}\"")).ok()
}
