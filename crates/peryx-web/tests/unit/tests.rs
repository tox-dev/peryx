use rstest::rstest;

use crate::model::{
    PolicyDecisionFilters, UiPolicyDecision, UiPolicyDecisionPage, UiSearchPage, UiSearchResult,
    blob_placement_status_label,
};
use peryx_core::BlobPlacementStatus;

fn policy_decision(state: &str, fresh: bool) -> UiPolicyDecision {
    UiPolicyDecision {
        id: "decision-1".to_owned(),
        repository: "private".to_owned(),
        resource: "example".to_owned(),
        group: Some("1.0".to_owned()),
        artifact: Some("example-1.0.bin".to_owned()),
        source: Some("alpha".to_owned()),
        action: "serve".to_owned(),
        state: state.to_owned(),
        rule: Some("blocked-resource".to_owned()),
        reason: Some("resource is blocked".to_owned()),
        evaluated_at_unix: 0,
        next_eligible_at_unix: None,
        fresh,
    }
}

#[rstest]
#[case::allow("allow", true, "Allowed")]
#[case::deny("deny", true, "Denied")]
#[case::wait("wait", true, "Waiting")]
#[case::stale("allow", false, "Stale Allowed")]
#[case::unknown("future", true, "Unknown")]
fn test_policy_decision_status(#[case] state: &str, #[case] fresh: bool, #[case] expected: &str) {
    assert_eq!(policy_decision(state, fresh).status(), expected);
}

#[test]
fn test_policy_decision_formats_times() {
    let mut decision = policy_decision("wait", true);
    decision.next_eligible_at_unix = Some(60);
    assert_eq!(decision.evaluated_at(), "1970-01-01T00:00:00Z");
    assert_eq!(decision.next_eligible_at(), "1970-01-01T00:01:00Z");
    decision.next_eligible_at_unix = None;
    assert_eq!(decision.next_eligible_at(), "-");
    decision.evaluated_at_unix = i64::MAX;
    assert_eq!(decision.evaluated_at(), i64::MAX.to_string());
    decision.evaluated_at_unix = -62_198_841_600;
    assert_eq!(decision.evaluated_at(), "-62198841600");
}

#[test]
fn test_policy_decision_filters_build_encoded_url() {
    let filters = PolicyDecisionFilters {
        repository: "team/private".to_owned(),
        state: "deny".to_owned(),
        rule: "blocked resource".to_owned(),
        source: "alpha".to_owned(),
        from: "1970-01-01T00:01".to_owned(),
        to: "1970-01-01T00:02".to_owned(),
        limit: "50".to_owned(),
    };
    assert_eq!(
        filters.url(Some("next page")).unwrap(),
        "/+policy/decisions?repository=team%2Fprivate&state=deny&rule=blocked+resource&source=alpha&from=60&to=120&limit=50&cursor=next+page"
    );
}

#[test]
fn test_policy_decision_filters_reject_invalid_datetime() {
    assert_eq!(
        PolicyDecisionFilters::default().url(None).unwrap(),
        "/+policy/decisions?limit=25"
    );
    let filters = PolicyDecisionFilters {
        from: "not-a-date".to_owned(),
        ..PolicyDecisionFilters::default()
    };
    assert_eq!(
        filters.url(None),
        Err("Invalid UTC date and time: not-a-date".to_owned())
    );
}

#[test]
fn test_policy_decision_page_deserializes_api_response() {
    let page: UiPolicyDecisionPage = serde_json::from_value(serde_json::json!({
        "decisions": [{
            "id": "decision-1", "repository": "private", "resource": "example", "version": null,
            "artifactname": null, "source": null, "action": "serve", "state": "allow", "rule": null,
            "reason": null, "evaluated_at_unix": 0, "input_generation": {"repository": 0},
            "next_eligible_at_unix": null, "fresh": true
        }],
        "next_cursor": "next"
    }))
    .unwrap();
    assert_eq!(page.decisions[0].status(), "Allowed");
    assert_eq!(page.next_cursor.as_deref(), Some("next"));
}

#[rstest]
#[case::verified(BlobPlacementStatus::Verified, "Verified", "health-live")]
#[case::pending(BlobPlacementStatus::Pending, "Pending", "health-unready")]
#[case::failed(BlobPlacementStatus::Failed, "Failed", "health-unknown")]
#[case::revoked(BlobPlacementStatus::Revoked, "Revoked", "health-restricted")]
fn test_blob_placement_status_label(
    #[case] status: BlobPlacementStatus,
    #[case] text: &'static str,
    #[case] class: &'static str,
) {
    assert_eq!(
        blob_placement_status_label(status),
        crate::model::HealthLabel { text, class }
    );
}

#[test]
fn test_operation_status_labels() {
    assert_eq!(
        [
            crate::model::UiOperationStatus::Pending,
            crate::model::UiOperationStatus::Published,
            crate::model::UiOperationStatus::Failed,
            crate::model::UiOperationStatus::Expired,
        ]
        .map(crate::model::operation_status_label),
        [
            crate::model::HealthLabel {
                text: "Pending",
                class: "health-unready",
            },
            crate::model::HealthLabel {
                text: "Published",
                class: "health-live",
            },
            crate::model::HealthLabel {
                text: "Failed",
                class: "health-unknown",
            },
            crate::model::HealthLabel {
                text: "Expired",
                class: "health-restricted",
            },
        ]
    );
}

#[test]
fn test_search_page_from_json() {
    let value = serde_json::json!({
        "query": "artifact-a",
        "type": "override",
        "availability": "local",
        "page": 2,
        "page_size": 50,
        "total": 51,
        "results": [{
            "display_label": "Artifact A",
            "resource_key": "artifact-a",
            "route": "root/alpha",
            "index": "root/alpha",
            "ecosystem": "alpha",
            "type_label": "artifact",
            "type": "override",
            "available": true,
            "summary": "web framework",
        }, {
            "display_label": "Artifact B",
            "resource_key": "artifact-b",
            "route": "root/alpha",
            "index": "root/alpha",
            "ecosystem": "alpha",
            "type_label": "artifact",
            "type": "cached",
            "available": false,
        }],
    });
    let page: UiSearchPage = serde_json::from_value(value).expect("well-formed response parses");
    assert_eq!(page.query, "artifact-a");
    assert_eq!(page.availability, "local");
    assert_eq!(page.page, 2);
    assert_eq!(page.results[0].source_label(), "Override");
    assert!(page.results[0].available);
    assert_eq!(page.results[0].summary.as_deref(), Some("web framework"));
    assert_eq!(page.results[1].summary, None);
    assert!(!page.results[1].available);
}

#[rstest]
#[case::uploaded("uploaded", "Uploaded")]
#[case::override_source("override", "Override")]
#[case::cached("cached", "Cached")]
#[case::unknown("future", "Cached")]
fn test_search_result_labels_source(#[case] source_type: &str, #[case] expected: &str) {
    assert_eq!(crate::model::source_label(source_type), expected);
}

#[test]
fn test_search_page_accepts_empty_results() {
    let value = serde_json::json!({
        "query": "",
        "type": "all",
        "availability": "all",
        "page": 1,
        "page_size": 25,
        "total": 0,
        "results": [],
    });
    let page: UiSearchPage = serde_json::from_value(value).expect("a valid empty response parses");
    assert_eq!(page.total, 0);
    assert!(page.results.is_empty());
}

#[rstest]
#[case::empty_object(serde_json::json!({}))]
#[case::missing_pagination(serde_json::json!({
    "query": "artifact-a",
    "type": "all",
    "availability": "all",
    "total": 0,
    "results": [],
}))]
#[case::wrong_scalar_type(serde_json::json!({
    "query": "artifact-a",
    "type": "all",
    "availability": "all",
    "page": "2",
    "page_size": 25,
    "total": 0,
    "results": [],
}))]
#[case::malformed_result(serde_json::json!({
    "query": "artifact-a",
    "type": "all",
    "availability": "all",
    "page": 1,
    "page_size": 25,
    "total": 1,
    "results": [{
        "display_label": "Artifact A",
        "resource_key": "artifact-a",
        "route": "root/alpha",
        "type": "cached",
    }],
}))]
fn test_search_page_rejects_malformed(#[case] value: serde_json::Value) {
    assert!(serde_json::from_value::<UiSearchPage>(value).is_err());
}

fn search_page(page: usize, page_size: usize, total: usize, results: usize) -> UiSearchPage {
    UiSearchPage {
        query: "artifact-a".to_owned(),
        source_type: "all".to_owned(),
        availability: "all".to_owned(),
        page,
        page_size,
        total,
        results: (0..results)
            .map(|index| UiSearchResult {
                display_label: "Artifact A".to_owned(),
                resource_key: "artifact-a".to_owned(),
                route: "root/alpha".to_owned(),
                index: index.to_string(),
                ecosystem: "alpha".to_owned(),
                type_label: "artifact".to_owned(),
                source_type: "cached".to_owned(),
                available: true,
                summary: None,
            })
            .collect(),
    }
}

#[rstest]
#[case::first_full(1, 25, 100, 25, Some((1, 25)))]
#[case::last_partial(4, 25, 76, 1, Some((76, 76)))]
#[case::single(1, 25, 1, 1, Some((1, 1)))]
#[case::out_of_range(999, 25, 1, 0, None)]
fn test_search_page_shown_range(
    #[case] page: usize,
    #[case] page_size: usize,
    #[case] total: usize,
    #[case] results: usize,
    #[case] expected: Option<(usize, usize)>,
) {
    assert_eq!(search_page(page, page_size, total, results).shown_range(), expected);
}

#[test]
fn test_stats_routes_sums_all_counters() {
    let value = serde_json::json!({
        "root/alpha": {
            "base": {"pages": 1, "reads": 2, "bytes": 3, "rejected": 4},
            "ecosystem": {"metadata": 5},
            "hosted": {"writes": 6},
            "cached": {
                "refreshes": 7,
                "changed": 8,
                "stale_served": 9,
                "upstream_errors": 10
            }
        },
    });
    assert_eq!(
        crate::model::stats_routes(&value).totals,
        crate::model::UiCounters {
            pages: 1,
            reads: 2,
            metadata: 5,
            writes: 6,
            bytes: 3,
            refreshes: 7,
            changed: 8,
            stale_served: 9,
            upstream_errors: 10,
            rejected: 4,
        }
    );
}

#[test]
fn test_stats_routes_sorts_by_total_activity() {
    let stats = crate::model::stats_routes(&serde_json::json!({
        "higher-sum": {"base": {"pages": 10, "reads": 0}},
        "higher-product": {"base": {"pages": 3, "reads": 3}},
    }));
    assert_eq!(
        stats.rows.into_iter().map(|(name, _)| name).collect::<Vec<_>>(),
        ["higher-sum", "higher-product"]
    );
}

#[test]
fn test_stats_index_reads_totals_and_resources() {
    let value = serde_json::json!({
        "totals": {
            "base": {"pages": 4, "reads": 2, "rejected": 1},
            "cached": {"stale_served": 1, "upstream_errors": 1}
        },
        "resources": {
            "resource-a": {"base": {"pages": 3, "reads": 2, "bytes": 500}},
            "resource-b": {"base": {"pages": 1, "reads": 0}},
        },
    });
    let stats = crate::model::stats_index(&value);
    assert_eq!(stats.totals.stale_served, 1);
    assert_eq!(stats.totals.upstream_errors, 1);
    assert_eq!(stats.totals.rejected, 1);
    assert_eq!(stats.rows[0].0, "resource-a");
    assert_eq!(stats.rows[0].1.bytes, 500);
}

#[test]
fn test_stats_resource_reads_grouped_totals_and_artifacts() {
    let value = serde_json::json!({
        "totals": {
            "base": {"pages": 3, "reads": 2, "bytes": 500},
            "ecosystem": {"metadata": 2}
        },
        "artifacts": {
            "artifact.bin":
                {"reads": 2, "bytes": 500, "ecosystem": {"metadata": 2}},
        },
    });
    let stats = crate::model::stats_resource(&value);
    assert_eq!(stats.totals.reads, 2);
    assert_eq!(stats.totals.metadata, 2);
    assert_eq!(stats.rows.len(), 1);
    assert_eq!(stats.rows[0].1.metadata, 2);
}
