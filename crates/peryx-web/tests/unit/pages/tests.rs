use leptos::prelude::*;

use crate::data::{LoaderEndpoint, LoaderError};
use crate::model::{UiCounters, UiEcosystemSummary, UiMetricFamily, UiSnapshot, UiStats};

use super::{ErrorMessage, LoadState, ecosystem_stats, human_size, optional_counters_for, retain, usage_or_error};

#[test]
fn ecosystem_stats_render_declared_and_missing_families() {
    let html = ecosystem_stats(&UiSnapshot {
        ecosystems: vec![UiEcosystemSummary {
            ecosystem: "example".to_owned(),
            pages: 1,
            reads: 2,
            writes: 3,
            families: [("metadata".to_owned(), 4)].into(),
            ..UiEcosystemSummary::default()
        }],
        families: vec![
            UiMetricFamily {
                ecosystem: "example".to_owned(),
                key: "metadata".to_owned(),
                label: "metadata hits".to_owned(),
                roles: Vec::new(),
            },
            UiMetricFamily {
                ecosystem: "example".to_owned(),
                key: "missing".to_owned(),
                label: "missing hits".to_owned(),
                roles: Vec::new(),
            },
        ],
        ..UiSnapshot::default()
    })
    .to_html();
    assert!(html.contains("example"), "{html}");
    assert!(html.contains("metadata hits"), "{html}");
    assert!(html.contains("missing hits"), "{html}");
}

#[test]
fn optional_counters_find_exact_route() {
    let counters = UiCounters {
        pages: 7,
        ..UiCounters::default()
    };
    let usage = UiStats {
        rows: vec![("root/cache".to_owned(), counters)],
        ..UiStats::default()
    };
    assert_eq!(optional_counters_for(&usage, "root/cache"), Some(counters));
    assert_eq!(optional_counters_for(&usage, "cache"), None);
}

#[test]
fn error_message_escapes_text() {
    let html = view! { <ErrorMessage message="<failure>".to_owned() /> }.to_html();
    assert!(html.contains("&lt;failure&gt;"), "{html}");
}

#[test]
fn load_state_retains_value_during_failure() {
    Owner::new().with(|| {
        let state = RwSignal::new(LoadState::default());
        retain(state, Ok("available".to_owned()));
        let loaded = retain(state, Err(LoaderError::Request(LoaderEndpoint::Status)));
        assert_eq!(
            (loaded.value.as_deref(), loaded.error.as_deref()),
            (Some("available"), Some("Request to /+status failed."))
        );
    });
}

#[test]
fn usage_or_error_hands_a_read_half_to_the_page() {
    let usage = UiStats {
        totals: UiCounters {
            pages: 7,
            ..UiCounters::default()
        },
        ..UiStats::default()
    };
    assert_eq!(usage_or_error(Ok(usage.clone())), (Some(usage), None));
}

#[test]
fn usage_or_error_replaces_a_failed_half_with_its_message() {
    let failed = LoaderError::Status {
        endpoint: LoaderEndpoint::Stats,
        status: 503,
    };
    assert_eq!(
        usage_or_error(Err(failed)),
        (None, Some("/+stats returned HTTP 503.".to_owned()))
    );
}

#[test]
fn byte_sizes_choose_largest_unit() {
    for (bytes, expected) in [
        (1, "1.0 B"),
        (1_023, "1023.0 B"),
        (1_024, "1.0 kB"),
        (1_536, "1.5 kB"),
        (1_048_576, "1.0 MB"),
        (1_073_741_824, "1.0 GB"),
        (1_099_511_627_776, "1.0 TB"),
        (u64::MAX, "16777216.0 TB"),
    ] {
        assert_eq!(human_size(bytes), expected);
    }
}
