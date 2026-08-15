use std::cell::Cell;
use std::sync::atomic::Ordering;

use peryx_core::Ecosystem;
use peryx_driver::serving::{IndexSummary, RecentWrite};
use peryx_driver::state::{
    AppState, HostedDescription, Index, IndexDescription, IndexKind, SecretDescription, UpstreamDescription,
};
use peryx_http::response_security::FieldClassification;
use peryx_identity::IndexAcl;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::{
    has_administrator_access, has_operator_access, index_view, redacted_auth, snapshot_for_class, stats_for_class,
};
use crate::model::{UiEcosystemSummary, UiHosted, UiIndex, UiRecentWrite, UiUpstream};

#[test]
fn status_access_matches_field_classification() {
    for (class, expected) in [
        (FieldClassification::Public, (false, false)),
        (FieldClassification::Operator, (true, false)),
        (FieldClassification::Administrator, (true, true)),
    ] {
        assert_eq!(
            (has_operator_access(class), has_administrator_access(class)),
            expected,
            "class: {class:?}"
        );
    }
}

#[test]
fn auth_redaction_hides_configured_credentials() {
    for (auth, expected) in [
        ("none", None),
        ("basic", Some("<redacted>".to_owned())),
        ("bearer", Some("<redacted>".to_owned())),
    ] {
        assert_eq!(redacted_auth(auth), expected, "auth kind: {auth}");
    }
}

#[test]
fn snapshot_authority_controls_operational_fields_and_index_details() {
    let (_directory, app) = app();
    app.serving.requests.store(7, Ordering::Relaxed);

    for (class, recent_limit, operator, administrator) in [
        (FieldClassification::Public, Some(5), false, false),
        (FieldClassification::Operator, None, true, false),
        (FieldClassification::Administrator, Some(5), true, true),
    ] {
        let snapshot = snapshot_for_class(&app, class, recent_limit);
        let ecosystems = if operator {
            vec![UiEcosystemSummary {
                ecosystem: "fixture".to_owned(),
                pages: 0,
                reads: 0,
                bytes: 0,
                rejected: 0,
                writes: 0,
                families: std::collections::BTreeMap::default(),
            }]
        } else {
            Vec::new()
        };
        assert_eq!(
            (
                snapshot.requests,
                snapshot.ecosystems,
                snapshot.families,
                snapshot.indexes.len(),
                snapshot.indexes[0].hosted.is_some(),
                snapshot.indexes[0].endpoint.as_str(),
            ),
            (
                if operator { 7 } else { 0 },
                ecosystems,
                Vec::new(),
                1,
                administrator,
                "/hosted/"
            ),
            "class: {class:?}"
        );
    }
}

#[test]
fn index_projection_preserves_stats_and_redacts_administrator_secrets() {
    let projected = index_view(
        cached_description("bearer"),
        "https://packages.example/catalog".to_owned(),
        IndexSummary {
            resource_count: 3,
            write_count: 2,
            recent_writes: vec![RecentWrite {
                resource: "resource".to_owned(),
                artifact: "resource-1.bin".to_owned(),
                group: "release".to_owned(),
                written_at: Some("2026-08-10T00:00:00Z".to_owned()),
                size: Some(41),
            }],
        },
        true,
    );

    assert_eq!(
        projected,
        UiIndex {
            name: "cached".to_owned(),
            route: "cached".to_owned(),
            ecosystem: "fixture".to_owned(),
            endpoint: "https://packages.example/catalog".to_owned(),
            kind: "cached".to_owned(),
            layers: Vec::new(),
            uploads: false,
            upload_to: None,
            upstream: Some(UiUpstream {
                url: "https://packages.example".to_owned(),
                auth_kind: "bearer".to_owned(),
                auth_redacted: Some("<redacted>".to_owned()),
                status: "configured".to_owned(),
            }),
            hosted: None,
            resource_count: 3,
            write_count: 2,
            recent_writes: vec![UiRecentWrite {
                resource: "resource".to_owned(),
                artifact: "resource-1.bin".to_owned(),
                group: "release".to_owned(),
                written_at: Some("2026-08-10T00:00:00Z".to_owned()),
                size: Some(41),
            }],
        }
    );
}

#[test]
fn index_projection_hides_administrator_configuration() {
    assert_eq!(
        (
            index_view(
                cached_description("none"),
                "/cached/".to_owned(),
                IndexSummary::default(),
                false
            )
            .upstream,
            index_view(
                hosted_description(),
                "/hosted/".to_owned(),
                IndexSummary::default(),
                false
            )
            .hosted,
        ),
        (None, None)
    );
}

#[test]
fn index_projection_reports_hosted_secret_state() {
    assert_eq!(
        index_view(
            hosted_description(),
            "/hosted/".to_owned(),
            IndexSummary::default(),
            true
        )
        .hosted
        .unwrap(),
        UiHosted {
            volatile: true,
            token_configured: true,
            token_redacted: Some("<redacted>".to_owned()),
        }
    );
}

#[test]
fn stats_authority_gates_drill_results() {
    for (class, expected) in [
        (FieldClassification::Public, serde_json::json!({})),
        (FieldClassification::Operator, serde_json::json!({"reads": 4})),
        (FieldClassification::Administrator, serde_json::json!({"reads": 4})),
    ] {
        let called = Cell::new(false);
        assert_eq!(
            (
                stats_for_class(class, || {
                    called.set(true);
                    serde_json::json!({"reads": 4})
                }),
                called.get(),
            ),
            (expected, class != FieldClassification::Public)
        );
    }
}

fn app() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let app = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        vec![Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: Ecosystem::new("fixture"),
            kind: IndexKind::Hosted { volatile: true },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    (directory, app)
}

fn cached_description(auth: &'static str) -> IndexDescription {
    IndexDescription {
        name: "cached".to_owned(),
        route: "cached".to_owned(),
        ecosystem: "fixture".to_owned(),
        kind: "cached",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: Some(UpstreamDescription {
            url: "https://packages.example".to_owned(),
            auth,
            offline: false,
            status: "configured",
            sources: Vec::new(),
        }),
        hosted: None,
    }
}

fn hosted_description() -> IndexDescription {
    IndexDescription {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        ecosystem: "fixture".to_owned(),
        kind: "hosted",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: true,
        volatile_deletes: true,
        upload_to: None,
        upstream: None,
        hosted: Some(HostedDescription {
            volatile: true,
            upload_token: SecretDescription::new(true),
        }),
    }
}
