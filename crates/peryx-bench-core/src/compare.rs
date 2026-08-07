//! A/B compare of two reports: per-metric ratios, aggregated into one verdict the way the methodology
//! prescribes.
//!
//! A single peryx party is read from each report and its medians divided into a ratio oriented so
//! that above one always means slower/heavier. Cells the harness marked network-bound (dominated by
//! upstream variance peryx cannot control) or noisy (round-to-round spread too wide to trust) are
//! shown but left out of the gate. The kept ratios reduce to a geometric mean, the normalization-safe
//! way to average a suite of ratios, and a change past a practical threshold reads as a regression.

use std::path::Path;

use crate::report::{Report, Table, load, report_path};
use crate::stats::geometric_mean;

/// The party whose two revisions the compare weighs.
const PARTY: &str = "peryx";

/// Below this the geometric mean of the kept ratios is noise, not a regression: a laptop clears
/// Criterion's ±1% floor easily, so the bar sits at 3%.
const REGRESSION_THRESHOLD: f64 = 1.03;

/// One metric's before/after with the ratio oriented so above one is worse.
struct Change {
    table: String,
    row: String,
    base: f64,
    head: f64,
    worse: f64,
    gated: bool,
    reason: &'static str,
}

/// Compare the current report against `baseline`, print the per-metric table, and return whether the
/// gated geometric mean crossed the regression threshold.
///
/// # Errors
/// Returns an error when either report cannot be read.
pub fn against(baseline: &Path) -> anyhow::Result<bool> {
    let base = load(baseline)?;
    let head = load(&report_path())?;
    let changes = collect(&base, &head);
    Ok(verdict(&changes))
}

/// Every peryx metric present in both reports, oriented and gated.
fn collect(base: &Report, head: &Report) -> Vec<Change> {
    let mut changes = Vec::new();
    for (name, table) in &head.tables {
        let Some(base_table) = base.tables.get(name) else {
            continue;
        };
        let (Some(head_party), Some(base_party)) = (party(table), party(base_table)) else {
            continue;
        };
        for row in &table.rows {
            let Some(base_row) = base_table.rows.iter().find(|candidate| candidate.name == row.name) else {
                continue;
            };
            let (Some(head_value), Some(base_value)) = (row.cells[head_party].value, base_row.cells[base_party].value)
            else {
                continue;
            };
            let worse = if row.higher_is_better {
                base_value / head_value
            } else {
                head_value / base_value
            };
            let (gated, reason) = if row.network_bound {
                (false, "network")
            } else if row.cells[head_party].noisy || base_row.cells[base_party].noisy {
                (false, "noisy")
            } else {
                (true, "")
            };
            changes.push(Change {
                table: name.clone(),
                row: row.name.clone(),
                base: base_value,
                head: head_value,
                worse,
                gated,
                reason,
            });
        }
    }
    changes
}

/// The column of the peryx party in a table, if it ran.
fn party(table: &Table) -> Option<usize> {
    table.parties.iter().position(|entry| entry.name == PARTY)
}

/// Print the change table and return whether the gated geometric mean is a regression.
fn verdict(changes: &[Change]) -> bool {
    println!(
        "\n{:<18} {:<34} {:>12} {:>12} {:>9}  flag",
        "table", "metric", "base", "head", "change"
    );
    for change in changes {
        let delta = (change.worse - 1.0) * 100.0;
        let flag = if change.gated { "" } else { change.reason };
        println!(
            "{:<18} {:<34} {:>12.3} {:>12.3} {:>+8.1}%  {flag}",
            change.table, change.row, change.base, change.head, delta
        );
    }
    let kept: Vec<f64> = changes
        .iter()
        .filter(|change| change.gated)
        .map(|change| change.worse)
        .collect();
    let Some(overall) = geometric_mean(&kept) else {
        println!("\nno gated metrics to compare");
        return false;
    };
    let regression = overall > REGRESSION_THRESHOLD;
    let verdict = if regression { "REGRESSION" } else { "no regression" };
    println!(
        "\ngeometric mean over {} gated metrics: {:+.1}% ({verdict}, threshold {:+.0}%)",
        kept.len(),
        (overall - 1.0) * 100.0,
        (REGRESSION_THRESHOLD - 1.0) * 100.0,
    );
    regression
}
