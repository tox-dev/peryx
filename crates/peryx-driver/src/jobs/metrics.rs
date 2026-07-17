//! Lifecycle counters for the node-local job scheduler.
//!
//! The series are deliberately low-cardinality: labels carry only a job's static `kind` and a bounded
//! `outcome`/`reason`, never a scope or repository name. A run of any kind therefore adds a fixed,
//! small number of series, so the exposition stays flat as the store grows.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};

use crate::state::PrometheusSource;

/// How a job run ended, for the `finished` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
}

/// Why a submission never ran, for the `rejected` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Conflict,
    QueueFull,
}

/// The counters one job kind accumulates. Every field is emitted for every seen kind, so a single
/// populated kind exercises the whole exposition.
#[derive(Debug, Default, Clone, Copy)]
struct KindCounters {
    started: u64,
    succeeded: u64,
    failed: u64,
    cancelled: u64,
    conflict: u64,
    queue_full: u64,
    running: i64,
}

/// Process-wide counters the scheduler updates and the `/metrics` endpoint renders.
#[derive(Debug, Default)]
pub struct JobMetrics {
    kinds: Mutex<BTreeMap<&'static str, KindCounters>>,
}

impl JobMetrics {
    fn with<R>(&self, kind: &'static str, edit: impl FnOnce(&mut KindCounters) -> R) -> R {
        let mut kinds = self.kinds.lock().unwrap_or_else(PoisonError::into_inner);
        edit(kinds.entry(kind).or_default())
    }

    pub(crate) fn started(&self, kind: &'static str) {
        self.with(kind, |counters| {
            counters.started += 1;
            counters.running += 1;
        });
    }

    pub(crate) fn finished(&self, kind: &'static str, outcome: Outcome) {
        self.with(kind, |counters| {
            counters.running -= 1;
            match outcome {
                Outcome::Succeeded => counters.succeeded += 1,
                Outcome::Failed => counters.failed += 1,
                Outcome::Cancelled => counters.cancelled += 1,
            }
        });
    }

    pub(crate) fn rejected(&self, kind: &'static str, reason: Reject) {
        self.with(kind, |counters| match reason {
            Reject::Conflict => counters.conflict += 1,
            Reject::QueueFull => counters.queue_full += 1,
        });
    }
}

impl PrometheusSource for JobMetrics {
    fn write_metrics(&self, body: &mut String) {
        body.push_str(
            "# HELP peryx_jobs_started_total Node-local job runs started.\n\
             # TYPE peryx_jobs_started_total counter\n\
             # HELP peryx_jobs_finished_total Node-local job runs finished, by outcome.\n\
             # TYPE peryx_jobs_finished_total counter\n\
             # HELP peryx_jobs_rejected_total Node-local job submissions refused before running.\n\
             # TYPE peryx_jobs_rejected_total counter\n\
             # HELP peryx_jobs_running Node-local job runs currently executing.\n\
             # TYPE peryx_jobs_running gauge\n",
        );
        let kinds = self.kinds.lock().unwrap_or_else(PoisonError::into_inner);
        for (kind, counters) in kinds.iter() {
            let _ = write!(
                body,
                "peryx_jobs_started_total{{kind=\"{kind}\"}} {}\n\
                 peryx_jobs_finished_total{{kind=\"{kind}\",outcome=\"succeeded\"}} {}\n\
                 peryx_jobs_finished_total{{kind=\"{kind}\",outcome=\"failed\"}} {}\n\
                 peryx_jobs_finished_total{{kind=\"{kind}\",outcome=\"cancelled\"}} {}\n\
                 peryx_jobs_rejected_total{{kind=\"{kind}\",reason=\"conflict\"}} {}\n\
                 peryx_jobs_rejected_total{{kind=\"{kind}\",reason=\"queue_full\"}} {}\n\
                 peryx_jobs_running{{kind=\"{kind}\"}} {}\n",
                counters.started,
                counters.succeeded,
                counters.failed,
                counters.cancelled,
                counters.conflict,
                counters.queue_full,
                counters.running,
            );
        }
    }
}
