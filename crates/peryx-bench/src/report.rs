//! The TOML report zola renders: one file, one table per workload, merged across partial runs.

use std::path::PathBuf;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::servers::Server;
use crate::stats::Summary;
use crate::usage::Cost;

/// Best-in-row to worst-in-row tint names the site's stylesheet colors green through red.
const LADDER: &[&str] = &["faster", "par", "mild", "slow", "veryslow", "worst"];

/// The tint scale never compresses below an 8x span, so a near-parity row reads green throughout.
const MIN_SPAN: f64 = 2.079_441_541_679_835_9; // ln 8

/// The whole report: every workload's table, keyed by name.
#[derive(Deserialize)]
pub struct Report {
    #[serde(default)]
    pub tables: std::collections::BTreeMap<String, Table>,
}

/// Load a report from disk for an A/B compare.
///
/// # Errors
/// Returns an error when the file cannot be read or is not a valid report.
pub fn load(path: &std::path::Path) -> anyhow::Result<Report> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{} is not a valid report", path.display()))
}

/// One comparison table.
#[derive(Serialize, Deserialize)]
pub struct Table {
    pub label: String,
    pub baseline: String,
    pub parties: Vec<Party>,
    pub rows: Vec<Row>,
}

#[derive(Serialize, Deserialize)]
pub struct Party {
    pub name: String,
    pub url: String,
}

#[derive(Serialize, Deserialize)]
pub struct Row {
    pub name: String,
    pub cells: Vec<Cell>,
    /// The measurement is dominated by upstream/CDN variance peryx does not control (a cold,
    /// network-bound pass), so the site marks it and a regression check must not gate on it.
    #[serde(default)]
    pub network_bound: bool,
    /// Whether a larger number is the better one (a rate), so an A/B compare knows which direction a
    /// change means a regression.
    #[serde(default)]
    pub higher_is_better: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Cell {
    pub text: String,
    pub ratio: String,
    pub tint: String,
    /// The dispersion around `text` (the median), as `±CV%`; empty when a party has no number.
    #[serde(default)]
    pub spread: String,
    /// The observed `min–max` across the rounds; empty when a party has no number.
    #[serde(default)]
    pub range: String,
    /// The round-to-round spread is too wide to read this number as fact.
    #[serde(default)]
    pub noisy: bool,
    /// Rounds that landed past the Tukey fence, kept rather than dropped so the spread stays honest.
    #[serde(default)]
    pub outliers: usize,
    /// The raw median so an A/B compare can take ratios without parsing `text`; absent for a party
    /// with no number.
    #[serde(default)]
    pub value: Option<f64>,
}

/// What a `None` measurement means for a row, and how its cell renders.
#[derive(Clone, Copy)]
pub enum Absent {
    /// The party ran the workload and failed it.
    Failed,
    /// The party has nothing to measure (direct runs no server).
    NoServer,
}

/// How a row's numbers read.
#[derive(Clone, Copy)]
pub enum Metric {
    /// Wall-clock seconds; lower is better.
    Seconds,
    /// A rate in the named unit; higher is better.
    Rate(&'static str),
    /// A quantity in the named unit; lower is better.
    Amount(&'static str),
}

/// Format one local-serving row: median value with its spread, ratio against the baseline party, and
/// a best-to-worst tint. See [`build_row`].
pub fn row(name: &str, values: &[Option<Summary>], baseline: usize, metric: Metric, absent: Absent) -> Row {
    build_row(name, values, baseline, metric, absent, false)
}

/// Format one network-bound row (a cold pass whose time is dominated by the upstream, not peryx);
/// tinted and reported like any other but marked so a regression check skips it. See [`build_row`].
pub fn network_row(name: &str, values: &[Option<Summary>], baseline: usize, metric: Metric, absent: Absent) -> Row {
    build_row(name, values, baseline, metric, absent, true)
}

/// Format one row from each party's [`Summary`] over the run's rounds: the median is the point
/// estimate, its coefficient of variation is the spread, and a wide spread flags the cell as noisy.
///
/// The baseline is the no-proxy `direct` measurement where present, so every other cell reads as the
/// overhead (or win) a server adds on top of talking to the upstream itself. A `None` marks a party
/// without a number; `absent` says whether that is a failure (red) or a non-entity (plain).
///
/// A baseline that produced no number leaves the ratios out rather than aborting the run: `--only`
/// can leave the baseline party unselected, and a baseline whose every round failed should cost the
/// table its ratios, not the whole suite its results.
fn build_row(
    name: &str,
    values: &[Option<Summary>],
    baseline: usize,
    metric: Metric,
    absent: Absent,
    network_bound: bool,
) -> Row {
    let reference = values
        .get(baseline)
        .and_then(Option::as_ref)
        .map_or(f64::NAN, |summary| summary.median);
    let higher_is_better = matches!(metric, Metric::Rate(_));
    let cost = |value: f64| if higher_is_better { 1.0 / value } else { value };
    let finite: Vec<f64> = values.iter().flatten().map(|summary| cost(summary.median)).collect();
    let best = finite.iter().copied().fold(f64::INFINITY, f64::min);
    let worst = finite.iter().copied().fold(0.0f64, f64::max);
    let span = (worst / best).ln().max(MIN_SPAN);
    let cells = values
        .iter()
        .map(|value| {
            value.as_ref().map_or_else(
                || absent_cell(absent),
                |summary| {
                    let position = (cost(summary.median) / best).ln() / span;
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_precision_loss,
                        clippy::cast_sign_loss,
                        reason = "position is a small non-negative ladder fraction"
                    )]
                    let index = ((position * LADDER.len() as f64) as usize).min(LADDER.len() - 1);
                    Cell {
                        text: format_value(summary.median, metric),
                        ratio: if reference.is_finite() {
                            format!("{:.2}x", summary.median / reference)
                        } else {
                            "-".to_owned()
                        },
                        tint: LADDER[index].to_owned(),
                        spread: format_spread(summary),
                        range: format_range(summary, metric),
                        noisy: summary.noisy(),
                        outliers: summary.outliers,
                        value: Some(summary.median),
                    }
                },
            )
        })
        .collect();
    Row {
        name: name.to_owned(),
        cells,
        network_bound,
        higher_is_better,
    }
}

/// Reduce each party's per-round samples to a [`Summary`]. A party that failed every round has an
/// empty series and becomes `None`, which renders as an absent (failed) cell.
#[must_use]
pub fn summarize(samples: &[Vec<f64>]) -> Vec<Option<Summary>> {
    samples.iter().map(|series| Summary::of(series)).collect()
}

/// The dispersion note the site prints under the median: the coefficient of variation as a percent,
/// or empty when a single round leaves nothing to spread.
fn format_spread(summary: &Summary) -> String {
    if summary.n < 2 {
        return String::new();
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "cv percent is small"
    )]
    let percent = (summary.cv * 100.0).round() as u64;
    format!("±{percent}%")
}

/// The observed `min–max` band across the rounds, for the site to show behind the median; empty when
/// a single round leaves nothing to bound.
fn format_range(summary: &Summary, metric: Metric) -> String {
    if summary.n < 2 {
        return String::new();
    }
    format!(
        "{}–{}",
        format_value(summary.min, metric),
        format_value(summary.max, metric)
    )
}

const fn absent_kinds(absent: Absent) -> (&'static str, &'static str) {
    match absent {
        Absent::Failed => ("error", "worst"),
        Absent::NoServer => ("no server", "none"),
    }
}

fn absent_cell(absent: Absent) -> Cell {
    let (text, tint) = absent_kinds(absent);
    Cell {
        text: text.to_owned(),
        ratio: "n/a".to_owned(),
        tint: tint.to_owned(),
        spread: String::new(),
        range: String::new(),
        noisy: false,
        outliers: 0,
        value: None,
    }
}

/// The index of the no-proxy baseline party, `direct`; every ratio reads against it.
pub fn baseline(servers: &[Server]) -> usize {
    servers.iter().position(|server| server.name == "direct").unwrap_or(0)
}

/// The party resource rows compare against: direct runs no server, so it cannot anchor them.
pub fn anchor(servers: &[Server]) -> usize {
    servers
        .iter()
        .position(|server| server.name == "peryx")
        .unwrap_or_else(|| baseline(servers))
}

/// The rows every table ends with: what the server itself burned while the workload ran, summarized
/// across the rounds. Each party carries one [`Cost`] per round (`None` for `direct`, which runs no
/// server); the CPU seconds and peak resident memory are reduced to a median with its spread like any
/// other measurement.
pub fn cost_rows(servers: &[Server], costs: &[Option<Vec<Cost>>]) -> Vec<Row> {
    let anchor = anchor(servers);
    let cpu = summaries(costs, |cost| cost.cpu_seconds);
    #[expect(clippy::cast_precision_loss, reason = "resident sizes fit f64 to the byte")]
    let rss = summaries(costs, |cost| cost.peak_rss_bytes as f64 / 1e6);
    vec![
        row("server CPU", &cpu, anchor, Metric::Seconds, Absent::NoServer),
        row(
            "server peak memory",
            &rss,
            anchor,
            Metric::Amount("MB"),
            Absent::NoServer,
        ),
    ]
}

/// The cost rows for a closed-loop workload driven to a fixed clock, where the fastest server
/// necessarily completes the most requests.
///
/// Absolute CPU ranks such a table backwards: a server answering forty-seven times the requests burns
/// more of the processor doing it, and would score worst for being fastest. Divide by the work each
/// party actually did. Peak memory needs no such correction; it is a high-water mark, not a total.
#[must_use]
pub fn cost_rows_per_request(
    servers: &[Server],
    costs: &[Option<Vec<Cost>>],
    requests: &[Option<Vec<u64>>],
) -> Vec<Row> {
    let anchor = anchor(servers);
    let cpu: Vec<Option<Summary>> = costs
        .iter()
        .zip(requests)
        .map(|(party, served)| {
            let (party, served) = (party.as_ref()?, served.as_ref()?);
            #[expect(clippy::cast_precision_loss, reason = "request counts fit f64 exactly here")]
            let per_thousand: Vec<f64> = party
                .iter()
                .zip(served)
                .filter(|(_, count)| **count > 0)
                .map(|(cost, &count)| cost.cpu_seconds / count as f64 * 1000.0)
                .collect();
            Summary::of(&per_thousand)
        })
        .collect();
    #[expect(clippy::cast_precision_loss, reason = "resident sizes fit f64 to the byte")]
    let rss = summaries(costs, |cost| cost.peak_rss_bytes as f64 / 1e6);
    vec![
        row(
            "server CPU per 1k requests",
            &cpu,
            anchor,
            Metric::Seconds,
            Absent::NoServer,
        ),
        row(
            "server peak memory",
            &rss,
            anchor,
            Metric::Amount("MB"),
            Absent::NoServer,
        ),
    ]
}

/// Summarize one field of each party's per-round costs.
fn summaries(costs: &[Option<Vec<Cost>>], field: impl Fn(&Cost) -> f64) -> Vec<Option<Summary>> {
    costs
        .iter()
        .map(|party| {
            party
                .as_ref()
                .and_then(|rounds| Summary::of(&rounds.iter().map(&field).collect::<Vec<_>>()))
        })
        .collect()
}

/// Assemble a table over the run's parties.
pub fn table(label: &str, servers: &[Server], baseline: usize, rows: Vec<Row>) -> Table {
    Table {
        label: label.to_owned(),
        baseline: servers[baseline].name.to_owned(),
        parties: servers
            .iter()
            .map(|server| Party {
                name: server.name.to_owned(),
                url: server.homepage.to_owned(),
            })
            .collect(),
        rows,
    }
}

/// Merge `name` into the report on disk, keeping tables other runs produced.
///
/// # Errors
/// Returns an error when the existing report cannot be parsed or the new one cannot be written.
pub fn publish(name: &str, table: Table) -> anyhow::Result<()> {
    let path = report_path();
    let mut report: toml::Table = match std::fs::read_to_string(&path) {
        Ok(existing) => existing.parse().context("existing report is not valid TOML")?,
        Err(_) => toml::Table::new(),
    };
    let tables = report
        .entry("tables")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("`tables` is not a TOML table")?;
    tables.insert(name.to_owned(), toml::Value::try_from(table)?);
    std::fs::create_dir_all(path.parent().expect("the report lives under site/data"))?;
    std::fs::write(&path, toml::to_string_pretty(&report)?)?;
    println!("updated {} [{name}]", path.display());
    Ok(())
}

fn format_value(value: f64, metric: Metric) -> String {
    match metric {
        Metric::Seconds => format_seconds(value),
        Metric::Rate(unit) | Metric::Amount(unit) => format!("{} {unit}", thousands(value)),
    }
}

fn format_seconds(seconds: f64) -> String {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "wall clocks are non-negative and far below u64::MAX minutes"
    )]
    if seconds >= 60.0 {
        format!("{}m {:04.1}s", (seconds / 60.0) as u64, seconds % 60.0)
    } else if seconds >= 1.0 {
        format!("{seconds:.1} s")
    } else if seconds >= 0.01 {
        format!("{:.0} ms", seconds * 1000.0)
    } else if seconds >= 0.001 {
        format!("{:.1} ms", seconds * 1000.0)
    } else {
        // The per-endpoint rows land here, where rounding to whole milliseconds prints every one of
        // them as "0 ms" and erases the differences the table exists to show.
        format!("{:.0} µs", seconds * 1e6)
    }
}

/// Round to a whole number with `,` thousands separators so large rates stay readable.
fn thousands(value: f64) -> String {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "benchmark magnitudes are non-negative and far below u64::MAX"
    )]
    let whole = value.round() as u64;
    let digits = whole.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// Where the report lives: zola loads it relative to the site root.
pub fn report_path() -> PathBuf {
    repo_root().join("site").join("data").join("bench").join("report.toml")
}

/// The override an A/B sets so the peryx party launches from a specific binary (a base-commit
/// build) instead of this checkout's release binary.
fn binary_override() -> &'static std::sync::Mutex<Option<PathBuf>> {
    static OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> = std::sync::OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Point the peryx party at `path`, or clear the override with `None`. Set between the two runs of
/// an A/B so each measures a different build through the same harness.
pub fn set_peryx_binary(path: Option<PathBuf>) {
    *binary_override().lock().expect("binary override lock is not poisoned") = path;
}

/// The peryx binary the harness launches: the A/B override when one is set, otherwise the release
/// binary this checkout builds.
pub fn peryx_binary() -> PathBuf {
    binary_override()
        .lock()
        .expect("binary override lock is not poisoned")
        .clone()
        .unwrap_or_else(|| repo_root().join("target").join("release").join("peryx"))
}

/// The repository checkout root.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the crate lives two levels under the repository root")
        .to_path_buf()
}
