use rstest::rstest;

use crate::markdown::{external_link_rel, is_safe_artifact_link, is_safe_link};
use crate::model::{
    PolicyDecisionFilters, UiPolicyDecision, UiPolicyDecisionPage, UiSearchPage, UiSearchResult, UiShadowPage,
    UiSnapshot, members_from_listing, projects_from_list,
};

fn policy_decision(state: &str, fresh: bool) -> UiPolicyDecision {
    UiPolicyDecision {
        id: "decision-1".to_owned(),
        repository: "private".to_owned(),
        project: "example".to_owned(),
        version: Some("1.0".to_owned()),
        filename: Some("example-1.0.bin".to_owned()),
        source: Some("alpha".to_owned()),
        action: "serve".to_owned(),
        state: state.to_owned(),
        rule: Some("blocked-project".to_owned()),
        reason: Some("project is blocked".to_owned()),
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
        rule: "blocked project".to_owned(),
        source: "alpha".to_owned(),
        from: "1970-01-01T00:01".to_owned(),
        to: "1970-01-01T00:02".to_owned(),
        limit: "50".to_owned(),
    };
    assert_eq!(
        filters.url(Some("next page")).unwrap(),
        "/+policy/decisions?repository=team%2Fprivate&state=deny&rule=blocked+project&source=alpha&from=60&to=120&limit=50&cursor=next+page"
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
            "id": "decision-1", "repository": "private", "project": "example", "version": null,
            "filename": null, "source": null, "action": "serve", "state": "allow", "rule": null,
            "reason": null, "evaluated_at_unix": 0, "input_generation": {"repository": 0},
            "next_eligible_at_unix": null, "fresh": true
        }],
        "next_cursor": "next"
    }))
    .unwrap();
    assert_eq!(page.decisions[0].status(), "Allowed");
    assert_eq!(page.next_cursor.as_deref(), Some("next"));
}

#[test]
fn test_shadow_page_deserializes_api_response_and_defaults_absent_fields() {
    let page: UiShadowPage = serde_json::from_value(serde_json::json!({
        "candidates": [
            {"member": "hosted", "source": "hosted", "filename": "example-1.0.bin", "digest": "sha256:1", "selected": true},
            {"member": "alpha", "source": "cached", "filename": "example-1.0.bin", "selected": false, "reason": "precedence"}
        ],
        "next_cursor": null
    }))
    .unwrap();
    let (selected, shadowed) = (&page.candidates[0], &page.candidates[1]);
    assert_eq!(
        (selected.outcome(), selected.digest_text().as_str()),
        ("Selected", "sha256:1")
    );
    assert_eq!(
        (
            shadowed.outcome(),
            shadowed.reason_text().as_str(),
            shadowed.digest_text().as_str()
        ),
        ("Shadowed", "Higher-precedence member", "-")
    );
    assert!(page.next_cursor.is_none());
}

#[test]
fn test_snapshot_from_status_roundtrip() {
    let value = serde_json::json!({
        "version": "0.0.1",
        "serial": 7,
        "requests": 12,
        "by_ecosystem": [
            {"ecosystem": "alpha", "pages": 12, "downloads": 4, "bytes": 900, "rejected": 0,
             "uploads": 0, "families": {"metadata": 3}}
        ],
        "metric_families": [
            {"key": "metadata", "label": "metadata hits", "roles": ["cached", "hosted", "virtual"]}
        ],
        "indexes": [{
            "name": "alpha",
            "route": "alpha",
            "ecosystem": "alpha",
            "kind": "cached",
            "layers": [],
            "uploads": false,
            "upstream": {"url": "https://upstream.example/artifacts/", "auth": {"kind": "none"}, "status": "configured"},
            "project_count": 2,
            "upload_count": 0,
            "recent_uploads": [],
        }],
    });
    let snapshot = UiSnapshot::from_status(&value);
    assert_eq!(snapshot.version, "0.0.1");
    assert_eq!(snapshot.serial, 7);
    assert_eq!(snapshot.requests, 12);
    assert_eq!(snapshot.ecosystems.len(), 1);
    assert_eq!(snapshot.ecosystems[0].ecosystem, "alpha");
    assert_eq!(snapshot.ecosystems[0].families["metadata"], 3);
    assert_eq!(snapshot.families[0].label, "metadata hits");
    assert_eq!(snapshot.indexes.len(), 1);
    assert_eq!(snapshot.indexes[0].kind, "cached");
    assert_eq!(snapshot.indexes[0].project_count, 2);
    assert_eq!(
        snapshot.indexes[0].upstream.as_ref().unwrap().url,
        "https://upstream.example/artifacts/"
    );
}

#[rstest]
#[case::hosted(crate::model::UiArtifactSource::Hosted, "hosted", "hosted")]
#[case::proxy(crate::model::UiArtifactSource::Proxy, "proxy", "proxy")]
#[case::generated(crate::model::UiArtifactSource::Generated, "generated", "generated")]
fn test_file_source_label_words_each_source(
    #[case] source: crate::model::UiArtifactSource,
    #[case] key: &str,
    #[case] text: &str,
) {
    let label = crate::model::file_source_label(source);
    assert_eq!(label.key, key);
    assert_eq!(label.text, text);
    assert!(!label.hint.is_empty());
}

#[rstest]
#[case::local(crate::model::UiByteAvailability::Local, "local", "local")]
#[case::remote_only(crate::model::UiByteAvailability::RemoteOnly, "remote-only", "remote-only")]
#[case::unavailable(crate::model::UiByteAvailability::Unavailable, "unavailable", "unavailable")]
fn test_byte_availability_label_words_each_state(
    #[case] availability: crate::model::UiByteAvailability,
    #[case] key: &str,
    #[case] text: &str,
) {
    let label = crate::model::byte_availability_label(availability);
    assert_eq!(label.key, key);
    assert_eq!(label.text, text);
    assert!(!label.hint.is_empty());
}

#[test]
fn test_placement_labels_are_all_distinct_text() {
    let mut texts = std::collections::BTreeSet::new();
    for source in [
        crate::model::UiArtifactSource::Hosted,
        crate::model::UiArtifactSource::Proxy,
        crate::model::UiArtifactSource::Generated,
    ] {
        texts.insert(crate::model::file_source_label(source).text);
    }
    for availability in [
        crate::model::UiByteAvailability::Local,
        crate::model::UiByteAvailability::RemoteOnly,
        crate::model::UiByteAvailability::Unavailable,
    ] {
        texts.insert(crate::model::byte_availability_label(availability).text);
    }
    // Six placement dimensions, six distinct words: no state is told apart by colour alone.
    assert_eq!(texts.len(), 6);
}

#[test]
fn test_operation_status_labels_are_all_distinct_text() {
    let mut texts = std::collections::BTreeSet::new();
    for status in [
        crate::model::UiOperationStatus::Pending,
        crate::model::UiOperationStatus::Published,
        crate::model::UiOperationStatus::Failed,
        crate::model::UiOperationStatus::Expired,
    ] {
        texts.insert(crate::model::operation_status_label(status).text);
    }
    // Four statuses, four distinct words: no status is told apart by colour alone.
    assert_eq!(texts.len(), 4);
}

#[test]
fn test_projects_and_members_from_json() {
    let list = serde_json::json!({"projects": [{"name": "a"}, {"name": "b"}]});
    assert_eq!(projects_from_list(&list), ["a", "b"]);
    let listing =
        serde_json::json!({"members": [{"path": "x/METADATA", "size": 5, "kind": "text", "previewable": true}]});
    let members = members_from_listing(&listing);
    assert_eq!(members[0].path, "x/METADATA");
    assert_eq!(members[0].size, 5);
    assert_eq!(members[0].kind, "text");
    assert!(members[0].previewable);
}

#[test]
fn test_search_page_from_json() {
    let value = serde_json::json!({
        "query": "flask",
        "type": "override",
        "availability": "local",
        "page": 2,
        "page_size": 50,
        "total": 51,
        "results": [{
            "display_name": "Flask",
            "normalized_name": "flask",
            "route": "root/alpha",
            "index": "root/alpha",
            "ecosystem": "alpha",
            "type_label": "package",
            "type": "override",
            "available": true,
            "summary": "web framework",
        }, {
            "display_name": "Django",
            "normalized_name": "django",
            "route": "root/alpha",
            "index": "root/alpha",
            "ecosystem": "alpha",
            "type_label": "package",
            "type": "cached",
            "available": false,
        }],
    });
    let page = UiSearchPage::from_search(&value).expect("well-formed response parses");
    assert_eq!(page.query, "flask");
    assert_eq!(page.availability, "local");
    assert_eq!(page.page, 2);
    assert_eq!(page.results[0].source_label(), "Override");
    assert!(page.results[0].available);
    assert_eq!(page.results[0].summary.as_deref(), Some("web framework"));
    // The optional summary is the only result field that may be absent.
    assert_eq!(page.results[1].summary, None);
    assert!(!page.results[1].available);
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
    let page = UiSearchPage::from_search(&value).expect("a valid empty response parses");
    assert_eq!(page.total, 0);
    assert!(page.results.is_empty());
}

#[rstest]
#[case::empty_object(serde_json::json!({}))]
#[case::missing_pagination(serde_json::json!({
    "query": "flask",
    "type": "all",
    "availability": "all",
    "total": 0,
    "results": [],
}))]
#[case::wrong_scalar_type(serde_json::json!({
    "query": "flask",
    "type": "all",
    "availability": "all",
    "page": "2",
    "page_size": 25,
    "total": 0,
    "results": [],
}))]
#[case::malformed_result(serde_json::json!({
    "query": "flask",
    "type": "all",
    "availability": "all",
    "page": 1,
    "page_size": 25,
    "total": 1,
    "results": [{
        "display_name": "Flask",
        "normalized_name": "flask",
        "route": "root/alpha",
        "type": "cached",
    }],
}))]
fn test_search_page_rejects_malformed(#[case] value: serde_json::Value) {
    let error = UiSearchPage::from_search(&value).expect_err("malformed response is rejected");
    assert!(error.starts_with("malformed search response:"), "{error}");
}

fn search_page(page: usize, page_size: usize, total: usize, results: usize) -> UiSearchPage {
    UiSearchPage {
        query: "flask".to_owned(),
        source_type: "all".to_owned(),
        availability: "all".to_owned(),
        page,
        page_size,
        total,
        results: (0..results)
            .map(|index| UiSearchResult {
                display_name: "Flask".to_owned(),
                normalized_name: "flask".to_owned(),
                route: "root/alpha".to_owned(),
                index: index.to_string(),
                ecosystem: "alpha".to_owned(),
                type_label: "package".to_owned(),
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
    let range = search_page(page, page_size, total, results).shown_range();
    assert_eq!(range, expected);
    if let Some((start, end)) = range {
        assert!(start <= end && end <= total);
    }
}

#[rstest]
#[case::http("http://example.com/docs", Some("external nofollow noopener noreferrer"))]
#[case::https("https://example.com/docs", Some("external nofollow noopener noreferrer"))]
#[case::mailto("mailto:maintainer@example.com", None)]
#[case::absolute_route("/alpha/files/veloxdemo-1.0.0.tar.gz", None)]
#[case::relative_route("../docs/", None)]
#[case::fragment("#usage", None)]
#[case::protocol_relative_attacker("//attacker.example/path", Some("external nofollow noopener noreferrer"))]
#[case::protocol_relative_loopback("//127.0.0.1/admin", Some("external nofollow noopener noreferrer"))]
#[case::malformed("http://[invalid", None)]
fn test_external_link_rel(#[case] target: &str, #[case] expected: Option<&str>) {
    assert_eq!(external_link_rel(target), expected);
}

#[rstest]
#[case::https("https://example.com/docs", true)]
#[case::mailto("mailto:maintainer@example.com", true)]
#[case::absolute_route("/alpha/files/veloxdemo-1.0.0.tar.gz", true)]
#[case::fragment("#usage", true)]
#[case::protocol_relative_attacker("//attacker.example/path", true)]
#[case::protocol_relative_loopback("//127.0.0.1/admin", true)]
#[case::javascript("javascript:alert(1)", false)]
fn test_is_safe_link(#[case] target: &str, #[case] expected: bool) {
    assert_eq!(is_safe_link(target), expected);
}

#[rstest]
#[case::https("https://example.com/pkg.bin", true)]
#[case::mailto("mailto:maintainer@example.com", false)]
#[case::absolute_route("/alpha/files/veloxdemo-1.0.0.tar.gz", true)]
#[case::fragment("#usage", true)]
#[case::protocol_relative_attacker("//attacker.example/pkg.bin", true)]
#[case::protocol_relative_loopback("//127.0.0.1/pkg.bin", true)]
#[case::javascript("javascript:alert(1)", false)]
fn test_is_safe_artifact_link(#[case] target: &str, #[case] expected: bool) {
    assert_eq!(is_safe_artifact_link(target), expected);
}

#[test]
fn test_stats_routes_sums_totals_and_sorts_busiest_first() {
    let value = serde_json::json!({
        "hosted": {"base": {"pages": 1, "downloads": 0, "bytes": 10}, "hosted": {"uploads": 2}},
        "root/alpha": {
            "base": {"pages": 5, "downloads": 3, "bytes": 900},
            "cached": {"refreshes": 2, "changed": 1}
        },
    });
    let stats = crate::model::stats_routes(&value);
    assert_eq!(stats.totals.pages, 6);
    assert_eq!(stats.totals.bytes, 910);
    assert_eq!(stats.totals.uploads, 2);
    assert_eq!(stats.totals.changed, 1);
    assert_eq!(stats.rows[0].0, "root/alpha");
    assert_eq!(stats.rows[1].0, "hosted");
}

#[test]
fn test_stats_index_reads_totals_and_projects() {
    let value = serde_json::json!({
        "totals": {
            "base": {"pages": 4, "downloads": 2, "rejected": 1},
            "cached": {"stale_served": 1, "upstream_errors": 1}
        },
        "projects": {
            "pandas": {"base": {"pages": 3, "downloads": 2, "bytes": 500}},
            "six": {"base": {"pages": 1, "downloads": 0}},
        },
    });
    let stats = crate::model::stats_index(&value);
    assert_eq!(stats.totals.stale_served, 1);
    assert_eq!(stats.totals.upstream_errors, 1);
    assert_eq!(stats.totals.rejected, 1);
    assert_eq!(stats.rows[0].0, "pandas");
    assert_eq!(stats.rows[0].1.bytes, 500);
}

#[test]
fn test_stats_project_reads_grouped_totals_and_files() {
    let value = serde_json::json!({
        "totals": {
            "base": {"pages": 3, "downloads": 2, "bytes": 500},
            "ecosystem": {"metadata": 2}
        },
        "files": {
            "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.bin":
                {"downloads": 2, "bytes": 500, "ecosystem": {"metadata": 2}},
        },
    });
    let stats = crate::model::stats_project(&value);
    assert_eq!(stats.totals.downloads, 2);
    assert_eq!(stats.totals.metadata, 2);
    assert_eq!(stats.rows.len(), 1);
    assert_eq!(stats.rows[0].1.metadata, 2);
}

#[rstest]
#[case::top("top", crate::model::AnalyticsView::Top, "/+analytics/top-packages")]
#[case::versions("versions", crate::model::AnalyticsView::Versions, "/+analytics/versions")]
#[case::sources("sources", crate::model::AnalyticsView::Sources, "/+analytics/sources")]
#[case::unused("unused", crate::model::AnalyticsView::Unused, "/+analytics/unused")]
#[case::timeline("timeline", crate::model::AnalyticsView::Timeline, "/+analytics/timeline")]
#[case::unknown_defaults_to_top("mystery", crate::model::AnalyticsView::Top, "/+analytics/top-packages")]
fn test_analytics_view_round_trips_query_and_path(
    #[case] query: &str,
    #[case] expected: crate::model::AnalyticsView,
    #[case] path: &str,
) {
    let view = crate::model::AnalyticsView::from_query(query);
    assert_eq!(view, expected);
    assert_eq!(view.path(), path);
}

#[test]
fn test_analytics_view_as_query_round_trips() {
    for view in [
        crate::model::AnalyticsView::Top,
        crate::model::AnalyticsView::Versions,
        crate::model::AnalyticsView::Sources,
        crate::model::AnalyticsView::Unused,
        crate::model::AnalyticsView::Timeline,
    ] {
        assert_eq!(crate::model::AnalyticsView::from_query(view.as_query()), view);
    }
}

#[test]
fn test_analytics_filters_build_encoded_url() {
    let filters = crate::model::AnalyticsFilters {
        view: "versions".to_owned(),
        repository: "team/private".to_owned(),
        from: "1970-01-02".to_owned(),
        to: "1970-01-03".to_owned(),
        limit: "50".to_owned(),
    };
    assert_eq!(
        filters.url(Some("cursor-1")).unwrap(),
        "/+analytics/versions?repository=team%2Fprivate&from=86400&to=172800&limit=50&cursor=cursor-1",
    );
}

#[test]
fn test_analytics_filters_omit_blank_optionals() {
    let filters = crate::model::AnalyticsFilters::default();
    assert_eq!(filters.url(None).unwrap(), "/+analytics/top-packages?limit=25");
}

#[test]
fn test_analytics_filters_reject_invalid_date() {
    let filters = crate::model::AnalyticsFilters {
        from: "not-a-date".to_owned(),
        ..crate::model::AnalyticsFilters::default()
    };
    assert_eq!(filters.url(None), Err("Invalid UTC date: not-a-date".to_owned()));
}

fn usage_envelope(rows_key: &str, rows: &serde_json::Value, next_cursor: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        rows_key: rows,
        "interval": {
            "from_day": 971,
            "to_day": 1000,
            "from_unix": 971 * 86_400,
            "to_unix": 1_001 * 86_400,
            "retained_from_day": null,
            "window_clamped_to_retention": false,
        },
        "next_cursor": next_cursor,
    })
}

#[test]
fn test_usage_page_parses_top_rows_and_cursor() {
    let value = usage_envelope(
        "packages",
        &serde_json::json!([{"repository": "root/alpha", "project": "veloxdemo", "downloads": 12, "bytes": 3400}]),
        &serde_json::json!("next-page"),
    );
    let page = crate::model::UiUsagePage::parse(crate::model::AnalyticsView::Top, &value).unwrap();
    assert_eq!(page.next_cursor.as_deref(), Some("next-page"));
    assert_eq!(page.rows.len(), 1);
    assert!(!page.rows.is_empty());
    let crate::model::UiUsageRows::Top(rows) = page.rows else {
        panic!("expected top rows");
    };
    assert_eq!(rows[0].downloads, 12);
    assert_eq!(rows[0].bytes, 3400);
}

#[test]
fn test_usage_page_parses_each_view() {
    let cases = [
        (
            crate::model::AnalyticsView::Versions,
            "versions",
            serde_json::json!([{"repository": "r", "project": "p", "version": null, "downloads": 1, "bytes": 2}]),
        ),
        (
            crate::model::AnalyticsView::Sources,
            "sources",
            serde_json::json!([{"repository": "r", "project": "p", "source": "alpha", "downloads": 1, "bytes": 2}]),
        ),
        (
            crate::model::AnalyticsView::Unused,
            "unused",
            serde_json::json!([{"repository": "r", "project": "p", "lifetime_downloads": 9}]),
        ),
        (
            crate::model::AnalyticsView::Timeline,
            "buckets",
            serde_json::json!([{"day": 1000, "start_unix": 86_400_000, "end_unix": 86_486_400, "downloads": 4, "bytes": 8}]),
        ),
    ];
    for (view, key, rows) in cases {
        let value = usage_envelope(key, &rows, &serde_json::Value::Null);
        let page = crate::model::UiUsagePage::parse(view, &value).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.next_cursor, None);
    }
}

#[test]
fn test_usage_page_parses_empty_rows() {
    let value = usage_envelope("packages", &serde_json::json!([]), &serde_json::Value::Null);
    let page = crate::model::UiUsagePage::parse(crate::model::AnalyticsView::Top, &value).unwrap();
    assert!(page.rows.is_empty());
}

#[rstest]
#[case::missing_interval(serde_json::json!({"packages": [], "next_cursor": null}))]
#[case::missing_rows(serde_json::json!({"interval": {"from_day": 0, "to_day": 0, "retained_from_day": null, "window_clamped_to_retention": false}, "next_cursor": null}))]
#[case::cursor_not_string(usage_envelope("packages", &serde_json::json!([]), &serde_json::json!(7)))]
#[case::row_wrong_shape(usage_envelope("packages", &serde_json::json!([{"repository": "r"}]), &serde_json::Value::Null))]
fn test_usage_page_rejects_malformed(#[case] value: serde_json::Value) {
    assert_eq!(
        crate::model::UiUsagePage::parse(crate::model::AnalyticsView::Top, &value),
        None
    );
}

#[test]
fn test_ui_interval_window_and_retention() {
    let clamped = crate::model::UiInterval {
        from_day: 100,
        to_day: 130,
        retained_from_day: Some(100),
        window_clamped_to_retention: true,
    };
    assert_eq!(clamped.window(), "1970-04-11 to 1970-05-11");
    assert_eq!(clamped.retained_from().as_deref(), Some("1970-04-11"));

    let open = crate::model::UiInterval {
        from_day: 0,
        to_day: 0,
        retained_from_day: None,
        window_clamped_to_retention: false,
    };
    assert_eq!(open.window(), "1970-01-01 to 1970-01-01");
    assert_eq!(open.retained_from(), None);
}

#[test]
fn test_format_instant_renders_utc_and_falls_back() {
    assert_eq!(crate::model::format_instant(0), "1970-01-01T00:00:00Z");
    assert_eq!(crate::model::format_instant(i64::MAX), i64::MAX.to_string());
}
