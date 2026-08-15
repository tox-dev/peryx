use std::path::{Path, PathBuf};

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
#[derive(Debug, PartialEq, Deserialize)]
pub struct Report {
    #[serde(default)]
    pub tables: std::collections::BTreeMap<String, Table>,
}

#[cfg(test)]
#[path = "../tests/unit/report.rs"]
mod tests;

/// # Errors
/// Returns an error when the file cannot be read or is not a valid report.
pub fn load(path: &std::path::Path) -> anyhow::Result<Report> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{} is not a valid report", path.display()))
}

/// One comparison table.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub label: String,
    pub baseline: String,
    pub parties: Vec<Party>,
    pub rows: Vec<Row>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    pub name: String,
    pub url: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub text: String,
    pub ratio: String,
    pub tint: String,
    /// The dispersion around `text` (the median), as `±CV%`; empty when a party has no number.
    #[serde(default)]
    pub spread: String,
    /// The observed `min..max` across the rounds; empty when a party has no number.
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

#[must_use]
pub fn row(name: &str, values: &[Option<Summary>], baseline: usize, metric: Metric, absent: Absent) -> Row {
    build_row(name, values, baseline, metric, absent, false)
}

#[must_use]
pub fn network_row(name: &str, values: &[Option<Summary>], baseline: usize, metric: Metric, absent: Absent) -> Row {
    build_row(name, values, baseline, metric, absent, true)
}

/// Missing baselines omit ratios so partial or failed baseline runs still produce a report.
fn build_row(
    name: &str,
    values: &[Option<Summary>],
    baseline: usize,
    metric: Metric,
    absent: Absent,
    network_bound: bool,
) -> Row {
    let anchor = values.get(baseline).and_then(Option::as_ref);
    let reference = anchor.map_or(f64::NAN, |summary| summary.median);
    let higher_is_better = matches!(metric, Metric::Rate(_));
    let cost = |value: f64| if higher_is_better { 1.0 / value } else { value };
    let finite: Vec<f64> = values.iter().flatten().map(|summary| cost(summary.median)).collect();
    let best = finite.iter().copied().fold(f64::INFINITY, f64::min);
    let worst = finite.iter().copied().fold(0.0f64, f64::max);
    let span = (worst / best).ln().max(MIN_SPAN);
    // The fastest resolved party for this row. Overlapping rounds do not establish a slower party.
    let leader = values
        .iter()
        .flatten()
        .min_by(|a, b| cost(a.median).total_cmp(&cost(b.median)));
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
                    let ties_leader = leader.is_some_and(|leader| indistinguishable(summary, leader));
                    Cell {
                        text: format_value(summary.median, metric),
                        ratio: if reference.is_finite() {
                            let approximate = anchor.is_some_and(|anchor| indistinguishable(summary, anchor));
                            let mark = if approximate { "\u{2248}" } else { "" };
                            format!("{mark}{:.2}x", summary.median / reference)
                        } else {
                            "-".to_owned()
                        },
                        tint: if ties_leader { LADDER[0] } else { LADDER[index] }.to_owned(),
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

#[must_use]
pub fn summarize(samples: &[Vec<f64>]) -> Vec<Option<Summary>> {
    samples.iter().map(|series| Summary::of(series)).collect()
}

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

fn format_range(summary: &Summary, metric: Metric) -> String {
    if summary.n < 2 {
        return String::new();
    }
    format!(
        "{}..{}",
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

#[must_use]
pub fn baseline(servers: &[Server]) -> usize {
    servers.iter().position(|server| server.name == "direct").unwrap_or(0)
}

/// The party resource rows compare against: direct runs no server, so it cannot anchor them.
#[must_use]
pub fn anchor(servers: &[Server]) -> usize {
    servers
        .iter()
        .position(|server| server.name == "peryx")
        .unwrap_or_else(|| baseline(servers))
}

#[must_use]
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

/// CPU totals penalize high throughput because faster servers handle more requests.
///
/// Normalize CPU by requests served. Peak memory remains absolute because it measures a high-water mark.
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

#[must_use]
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

/// # Errors
/// Returns an error when the report cannot be parsed or written.
pub fn publish_to(path: &Path, name: &str, table: Table) -> anyhow::Result<()> {
    let mut report: toml::Table = match std::fs::read_to_string(path) {
        Ok(existing) => existing.parse().context("existing report is not valid TOML")?,
        Err(_) => toml::Table::new(),
    };
    let tables = report
        .entry("tables")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("`tables` is not a TOML table")?;
    tables.insert(name.to_owned(), toml::Value::try_from(table)?);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&report)?)?;
    println!("updated {} [{name}]", path.display());
    Ok(())
}

/// Whether the rounds behind two cells overlap, so this run cannot tell the two apart.
///
/// A median is one number drawn from a spread. Reading `19.2 s` as faster than `20.5 s` when both
/// parties produced rounds covering the other's is reporting noise as a ranking, and a reader cannot
/// see it from the medians alone. Rows that overlap get a `~` on the ratio and share the leader's
/// colour, so only differences the run resolved are drawn as differences.
fn indistinguishable(one: &Summary, other: &Summary) -> bool {
    one.min <= other.max && other.min <= one.max
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

#[must_use]
pub fn report_path() -> PathBuf {
    repo_root().join("site").join("data").join("bench").join("report.toml")
}

#[must_use]
pub fn repo_root() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root.pop();
    root
}
