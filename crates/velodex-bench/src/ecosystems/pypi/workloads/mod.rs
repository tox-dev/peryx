//! The four workloads: installs, file throughput, a parallel CI fleet, and a request swarm.
//!
//! Every workload measures each server over `rounds` independent restarts: a fresh process and empty
//! state per round, so a cold pass is genuinely cold each time and the round-to-round spread captures
//! the between-launch variance (page cache, allocator layout, CPU frequency) that repeating inside one
//! process cannot see. The per-round samples reduce to a median with its dispersion (see
//! [`crate::report`] and [`crate::stats`]); the old best-of-N minimum is gone because its bias grows
//! with the round count and would make two runs of different lengths incomparable. Cold passes hit the
//! real upstream and are marked network-bound so a regression check skips their CDN-driven variance.

use std::process::Command;

use anyhow::{Context as _, bail};

use crate::usage::{Cost, Usage};

mod fleet;
mod install;
mod load;
mod metadata;
mod throughput;

pub use fleet::fleet;
pub use install::installs;
pub use load::load;
pub use metadata::metadata;
pub use throughput::throughput;

/// One server's per-round samples for a workload: a column of numbers per sub-metric, plus the
/// resource costs of the rounds that produced a server process.
struct Rounds {
    costs: Vec<Cost>,
}

impl Rounds {
    pub(super) const fn new() -> Self {
        Self { costs: Vec::new() }
    }

    /// Fold this round's cost in, dropping the `None` a serverless party (`direct`) reports.
    pub(super) fn record_cost(&mut self, usage: Usage) {
        if let Some(cost) = usage.finish() {
            self.costs.push(cost);
        }
    }

    /// The per-round costs, or `None` for a party that ran no server so cost rows read "no server".
    pub(super) fn costs(self) -> Option<Vec<Cost>> {
        (!self.costs.is_empty()).then_some(self.costs)
    }
}

/// Print one server's cold and warm medians so a live run is legible without the report.
pub(super) fn report_samples(label: &str, cold: &[f64], warm: &[f64]) {
    println!("{label}: cold {} warm {}", median_or_dash(cold), median_or_dash(warm));
}

fn median_or_dash(samples: &[f64]) -> String {
    crate::stats::Summary::of(samples).map_or_else(|| "-".to_owned(), |summary| format!("{:.1}s", summary.median))
}

pub(super) fn median_or_dash_rate(samples: &[f64]) -> String {
    crate::stats::Summary::of(samples).map_or_else(|| "-".to_owned(), |summary| format!("{:.0}", summary.median))
}

pub(super) fn run_checked(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output().context("command did not start")?;
    if !output.status.success() {
        bail!("{command:?} failed:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}
