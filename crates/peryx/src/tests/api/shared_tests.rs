use std::collections::BTreeSet;

use crate::api::{openapi, openapi_json};

// Keep sorted: a new endpoint inserts one line at its ordered position, so independent
// additions on separate PRs compose on merge instead of colliding on a shared path count.
#[test]
fn test_openapi_document_covers_every_endpoint() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let documented: BTreeSet<&str> = spec["paths"].as_object().unwrap().keys().map(String::as_str).collect();
    let expected = BTreeSet::from([
        "/+acl",
        "/+analytics/sources",
        "/+analytics/timeline",
        "/+analytics/top-packages",
        "/+analytics/unused",
        "/+analytics/versions",
        "/+api",
        "/+availability/topology",
        "/+grants",
        "/+grants/{id}",
        "/+health",
        "/+policy/decisions",
        "/+query",
        "/+quota",
        "/+quota/repository",
        "/+ready",
        "/+retention/export",
        "/+retention/plan",
        "/+revocations",
        "/+revocations/{digest}",
        "/+revocations/{digest}/lift",
        "/+search",
        "/+shadow/candidates",
        "/+stats",
        "/+status",
        "/+tokens",
        "/+tokens/{id}",
        "/+tokens/{id}/rotate",
        "/+trash",
        "/+trash/record",
        "/_/oidc/audience",
        "/_/oidc/mint-token",
        "/api-docs/openapi.json",
        "/metrics",
        "/v2/",
        "/v2/{name}/blobs/uploads/",
        "/v2/{name}/blobs/uploads/{session}",
        "/v2/{name}/blobs/{digest}",
        "/v2/{name}/blobs/{digest}/contents",
        "/v2/{name}/manifests/{reference}",
        "/v2/{name}/manifests/{reference}/restore",
        "/v2/{name}/referrers/{digest}",
        "/v2/{name}/tags/list",
        "/{route}/",
        "/{route}/+api",
        "/{route}/+search",
        "/{route}/files/{sha256}/{filename}",
        "/{route}/files/{sha256}/{filename}.metadata",
        "/{route}/inspect/{sha256}/{filename}",
        "/{route}/inspect/{sha256}/{filename}/{member}",
        "/{route}/simple/",
        "/{route}/simple/{project}/",
        "/{route}/{project}/",
        "/{route}/{project}/json",
        "/{route}/{project}/{version}/",
        "/{route}/{project}/{version}/json",
        "/{route}/{project}/{version}/promote",
        "/{route}/{project}/{version}/restore",
        "/{route}/{project}/{version}/yank",
    ]);
    assert_eq!(documented, expected);
    assert_eq!(spec["info"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn test_openapi_json_has_stable_object_order() {
    assert_json_objects_are_sorted(&serde_json::from_str(&openapi_json()).unwrap());
}

fn assert_json_objects_are_sorted(value: &serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => values.iter().for_each(assert_json_objects_are_sorted),
        serde_json::Value::Object(object) => {
            assert!(object.keys().is_sorted(), "object keys are not sorted: {object:?}");
            object.values().for_each(assert_json_objects_are_sorted);
        }
        _ => {}
    }
}

// The documentation site serves a checked-in copy rendered by ReDoc; regenerate it with
// `cargo run -p peryx -- openapi > site/static/openapi.json` whenever this test fails.
#[test]
fn test_site_openapi_copy_is_current() {
    // Normalized, so a checkout with CRLF line endings still compares content, not encoding.
    let site_copy = include_str!("../../../../../site/static/openapi.json").replace("\r\n", "\n");
    assert_eq!(site_copy, openapi_json(), "site/static/openapi.json is stale");
}
