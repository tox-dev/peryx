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

mod endpoints;
mod fleet;
mod install;
mod load;
mod metadata;
mod throughput;

pub use endpoints::endpoints;
pub use fleet::fleet;
pub use install::installs;
pub use load::load;
pub use metadata::metadata;
pub use throughput::throughput;

/// The interpreter every install workload builds its venv with.
///
/// Pinned so runs stay comparable as `uv venv`'s default interpreter moves. Installs also pass
/// `--only-binary :all:`: without it a package that lacks a wheel for the chosen interpreter is
/// compiled from source, and that build lands inside the measured install, dwarfing what the index
/// server contributes and swamping the machine while it runs. A missing wheel now fails the round
/// loudly instead of silently turning the benchmark into a compile.
const BENCH_PYTHON: &str = "3.14";

/// What pip and uv ask a simple index for, and so what any workload standing in for them must send.
///
/// peryx picks the representation by looking for `json` anywhere in `Accept`, so a workload sending
/// `*/*` silently measures the PEP 503 HTML render instead of the PEP 691 JSON an installer receives.
/// The two paths are not comparable: HTML re-parses and re-renders the page per request.
pub(super) const SIMPLE_ACCEPT: &str = "application/vnd.pypi.simple.v1+json, text/html;q=0.5";

/// The PEP 503 HTML an installer never asks for, but a browser and an old client do.
pub(super) const SIMPLE_ACCEPT_HTML: &str = "text/html";

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
