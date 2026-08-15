//! Seeded plans make availability failures reproducible across CI shards and local runs.
#![cfg(feature = "sim-campaign")]

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use peryx_ha_distributed::sim::{Config, Defect, Outcome, Topology, Trace, execute, generate_plan, minimize};

const MATRIX: &[(usize, usize)] = &[(2, 1), (3, 2), (4, 3), (5, 2)];

const DEFAULT_SEEDS: u64 = 512;
const DEFAULT_STEPS: usize = 200;
/// Bounds diagnostics when one defect breaks many seeds.
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
    let topology = topology(3, 2);
    let violating = (0..256)
        .map(|seed| Config {
            seed,
            topology,
            steps: DEFAULT_STEPS,
            defect: Some(Defect::ReapplyDuplicate),
        })
        .find(|config| !matches!(execute(config, &generate_plan(config)).outcome, Outcome::Held))
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

#[test]
fn test_campaign_inputs_parse() {
    assert_eq!(
        configured_topologies(None),
        MATRIX
            .iter()
            .map(|&(nodes, sources)| topology(nodes, sources))
            .collect::<Vec<_>>()
    );
    assert_eq!(configured_topologies(Some("3x2")), vec![topology(3, 2)]);
    assert_eq!(
        configured_artifacts(None),
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sim-campaign")
    );
    assert_eq!(configured_artifacts(Some("artifacts")), PathBuf::from("artifacts"));
    assert_eq!(parse_env_u64(None, 7), 7);
    assert_eq!(parse_env_u64(Some("11"), 7), 11);
    assert_eq!(parse_env_u64(Some("bad"), 7), 7);
    assert_eq!(parse_corpus("# kept\n\n3\n 5\n"), vec![3, 5]);
}

struct CampaignPlan {
    start: u64,
    seeds: u64,
    steps: usize,
    shard_index: u64,
    shard_total: u64,
    topologies: Vec<Topology>,
    artifacts: PathBuf,
    revision: String,
    /// `None` preserves the safety model; CI fault lanes disable one rule.
    defect: Option<Defect>,
}

impl CampaignPlan {
    fn from_env() -> Self {
        Self {
            start: env_u64("PERYX_SIM_START", 0),
            seeds: env_u64("PERYX_SIM_SEEDS", DEFAULT_SEEDS),
            steps: usize::try_from(env_u64("PERYX_SIM_STEPS", DEFAULT_STEPS as u64)).unwrap_or(DEFAULT_STEPS),
            shard_index: env_u64("PERYX_SIM_SHARD_INDEX", 0),
            shard_total: env_u64("PERYX_SIM_SHARD_TOTAL", 1).max(1),
            topologies: configured_topologies(env::var("PERYX_SIM_TOPOLOGY").ok().as_deref()),
            artifacts: configured_artifacts(env::var("PERYX_SIM_ARTIFACTS").ok().as_deref()),
            revision: env::var("GITHUB_SHA").unwrap_or_else(|_| "local".to_owned()),
            defect: env::var("PERYX_SIM_DEFECT").ok().as_deref().and_then(parse_defect),
        }
    }

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

    /// Keeps failure artifacts small enough to inspect and replay.
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
        let actions = failure.trace.actions.len();
        let artifact = failure.artifact.display();
        let steps = plan.steps;
        let outcome = &failure.trace.outcome;
        let _ = write!(
            out,
            "seed {seed} topology {nodes}x{sources}: {outcome:?} ({actions} minimized actions)\n  \
             trace: {artifact}\n  \
             repro: PERYX_SIM_START={seed} PERYX_SIM_SEEDS=1 PERYX_SIM_SHARD_TOTAL=1 PERYX_SIM_TOPOLOGY={nodes}x{sources} PERYX_SIM_STEPS={steps} cargo test -p peryx --features sim-campaign --test sim_campaign\n\n",
        );
    }
    out
}

fn load_corpus() -> Vec<u64> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/availability-seeds.txt");
    parse_corpus(&fs::read_to_string(&path).unwrap_or_default())
}

fn parse_corpus(text: &str) -> Vec<u64> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.parse().expect("corpus seed must be a u64"))
        .collect()
}

fn env_u64(key: &str, default: u64) -> u64 {
    parse_env_u64(env::var(key).ok().as_deref(), default)
}

fn parse_env_u64(value: Option<&str>, default: u64) -> u64 {
    value.and_then(|value| value.parse().ok()).unwrap_or(default)
}

fn configured_topologies(pinned: Option<&str>) -> Vec<Topology> {
    pinned.map_or_else(
        || {
            MATRIX
                .iter()
                .map(|&(nodes, sources)| topology(nodes, sources))
                .collect()
        },
        |pinned| vec![parse_topology(pinned)],
    )
}

fn configured_artifacts(path: Option<&str>) -> PathBuf {
    path.map_or_else(
        || PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sim-campaign"),
        PathBuf::from,
    )
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

/// Keeps environment values identical to serialized defect names.
fn parse_defect(name: &str) -> Option<Defect> {
    serde_json::from_str(&format!("\"{name}\"")).ok()
}
