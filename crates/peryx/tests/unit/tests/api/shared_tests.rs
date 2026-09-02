use std::collections::BTreeSet;

#[cfg(feature = "composition-pypi")]
use axum::body::Body;
#[cfg(feature = "composition-pypi")]
use axum::http::{Request, StatusCode, header};
#[cfg(feature = "composition-pypi")]
use base64::Engine as _;
#[cfg(feature = "composition-pypi")]
use http_body_util::BodyExt as _;
#[cfg(feature = "composition-pypi")]
use peryx_ecosystem_pypi::store::PypiStore as _;
#[cfg(feature = "composition-pypi")]
use peryx_identity::{GrantScope, Role};
#[cfg(feature = "composition-pypi")]
use tower::ServiceExt as _;
use utoipa::openapi::PathsBuilder;

#[cfg(feature = "composition-oci")]
use crate::api::openapi_with_plugins;
use crate::api::{openapi, openapi_for, openapi_json, openapi_json_for};

#[test]
#[cfg(feature = "composition-oci")]
fn test_oci_only_openapi_omits_the_pypi_shadow_path() {
    let plugins = peryx_plugin_registry::PluginRegistry::new(vec![peryx_ecosystem_oci::registration()]).unwrap();
    let spec = serde_json::to_value(openapi_with_plugins(&plugins)).unwrap();

    assert!(spec["paths"].get("/+shadow/candidates").is_none());
}

// Sorted entries reduce merge conflicts between endpoint additions.
#[test]
fn test_openapi_document_covers_every_endpoint() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let documented: BTreeSet<String> = spec["paths"].as_object().unwrap().keys().cloned().collect();
    let plugin_spec =
        serde_json::to_value(crate::compiled_plugins().openapi_paths(PathsBuilder::new()).build()).unwrap();
    let plugin_paths: BTreeSet<String> = plugin_spec.as_object().unwrap().keys().cloned().collect();
    let core_paths: BTreeSet<String> = documented.difference(&plugin_paths).cloned().collect();
    let expected = BTreeSet::from(
        [
            "/+acl",
            "/+analytics/completeness",
            "/+analytics/sources",
            "/+analytics/timeline",
            "/+analytics/top-resources",
            "/+analytics/unused",
            "/+analytics/groups",
            "/+api",
            "/+availability/operations",
            "/+availability/placements",
            "/+availability/placements/{digest}",
            "/+availability/topology",
            "/+availability/topology/stream",
            "/+cache",
            "/+cache/fsck",
            "/+cache/purge",
            "/+cache/size",
            "/+grants",
            "/+grants/{id}",
            "/+health",
            "/+jobs/{id}/cancel",
            "/+policy/decisions",
            "/+query",
            "/+quota",
            "/+quota/repository",
            "/+ready",
            "/+repositories",
            "/+repositories/{id}",
            "/+repositories/{id}/disable",
            "/+repositories/{id}/enable",
            "/+retention/export",
            "/+retention/plan",
            "/+revocations",
            "/+revocations/{digest}",
            "/+revocations/{digest}/lift",
            "/+search",
            "/+stats",
            "/+status",
            "/+tokens",
            "/+tokens/{id}",
            "/+tokens/{id}/rotate",
            "/+trash",
            "/+trash/record",
            "/api-docs/openapi.json",
            "/metrics",
        ]
        .map(str::to_owned),
    );
    assert_eq!(core_paths, expected);
    assert_eq!(spec["info"]["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
#[cfg(feature = "composition-pypi")]
async fn test_shadow_contract_openapi_matches_the_public_handler() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let operation = &spec["paths"]["/+shadow/candidates"]["get"];
    let parameters = operation["parameters"].as_array().unwrap();
    assert_eq!(
        parameters
            .iter()
            .map(|parameter| parameter["name"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["cursor", "limit", "project", "repository"])
    );

    let directory = tempfile::tempdir().unwrap();
    let state = crate::server::build_state(&crate::config::Config {
        data_dir: directory.path().to_path_buf(),
        ..crate::config::Config::default()
    })
    .unwrap();
    seed_shadow_candidate(&state);
    let user = state.serving.users.create("OpenAPI reader").unwrap();
    state.serving.users.set_password(&user.id, "password").await.unwrap();
    state
        .serving
        .authorization
        .grant(
            &user.id,
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "root-pypi".to_owned(),
            },
        )
        .unwrap();
    let query = parameters
        .iter()
        .filter(|parameter| parameter["required"] == true)
        .map(|parameter| {
            (
                parameter["name"].as_str().unwrap(),
                parameter["example"].as_str().unwrap(),
            )
        });
    let request = Request::builder()
        .uri(format!(
            "/+shadow/candidates?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query)
                .finish()
        ))
        .header(
            header::AUTHORIZATION,
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("OpenAPI reader:password")
            ),
        )
        .body(Body::empty())
        .unwrap();
    let response = crate::server::router_for(state, axum::Router::new())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        body["candidates"][0]
            .as_object()
            .unwrap()
            .keys()
            .collect::<BTreeSet<_>>(),
        operation["responses"]["200"]["content"]["application/json"]["example"]["candidates"][0]
            .as_object()
            .unwrap()
            .keys()
            .collect()
    );
}

#[test]
fn test_repository_state_filter_has_a_closed_enum() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let state = spec["paths"]["/+repositories"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "state")
        .unwrap();

    assert_eq!(
        state["schema"],
        serde_json::json!({"type": "string", "enum": ["enabled", "disabled"]})
    );
}

#[test]
fn test_openapi_parameters_declare_exactly_one_shape() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let invalid = spec["paths"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(path, item)| {
            item.as_object().unwrap().iter().flat_map(move |(method, operation)| {
                operation["parameters"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(move |parameter| {
                        ["schema", "content"]
                            .into_iter()
                            .filter(|field| parameter.get(field).is_some())
                            .count()
                            != 1
                    })
                    .map(move |parameter| (method, path, &parameter["in"], &parameter["name"]))
            })
        })
        .collect::<Vec<_>>();

    assert!(invalid.is_empty(), "parameters without one shape: {invalid:?}");
}

#[test]
fn test_openapi_path_parameters_match_templates() {
    let spec = serde_json::to_value(openapi()).unwrap();

    for (path, item) in spec["paths"].as_object().unwrap() {
        let templates = path
            .split('{')
            .skip(1)
            .map(|suffix| suffix.split_once('}').unwrap().0)
            .collect::<BTreeSet<_>>();
        for (method, operation) in item.as_object().unwrap() {
            let parameters = operation["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|parameter| parameter["in"] == "path")
                .collect::<Vec<_>>();
            let declared = parameters
                .iter()
                .map(|parameter| parameter["name"].as_str().unwrap())
                .collect::<BTreeSet<_>>();

            assert_eq!(declared, templates, "{method} {path}");
            assert!(
                parameters
                    .iter()
                    .all(|parameter| parameter["required"].as_bool().is_some_and(|required| required))
            );
        }
    }
}

#[test]
fn test_openapi_parameter_schemas_match_request_contracts() {
    let spec = serde_json::to_value(openapi()).unwrap();

    for (path, name, expected) in [
        (
            "/+search",
            "type",
            serde_json::json!({"type": "string", "enum": ["uploaded", "cached", "override"]}),
        ),
        ("/+search", "page", serde_json::json!({"type": "integer", "minimum": 1})),
        (
            "/+search",
            "page_size",
            serde_json::json!({"type": "integer", "enum": [25, 50, 100]}),
        ),
        (
            "/+repositories",
            "state",
            serde_json::json!({"type": "string", "enum": ["enabled", "disabled"]}),
        ),
        (
            "/+repositories",
            "limit",
            serde_json::json!({"type": "integer", "minimum": 1, "maximum": 100}),
        ),
        ("/+ready", "writes", serde_json::json!({"type": "boolean"})),
        (
            concat!("/v2/", "{name}", "/tags/list"),
            "n",
            serde_json::json!({"type": "integer", "minimum": 0}),
        ),
        (
            "/{route}/inspect/{sha256}/{filename}",
            "container",
            serde_json::json!({"type": "array", "items": {"type": "string"}}),
        ),
        (
            "/{route}/inspect/{sha256}/{filename}",
            "limit",
            serde_json::json!({"type": "integer", "minimum": 1, "maximum": 1_048_576}),
        ),
    ] {
        assert_eq!(openapi_parameter(&spec, path, "get", name)["schema"], expected);
    }
}

#[test]
fn test_openapi_repeated_query_parameter_uses_default_serialization() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let parameter = openapi_parameter(&spec, "/{route}/inspect/{sha256}/{filename}", "get", "container");

    assert!(parameter.get("style").is_none());
    assert!(parameter.get("explode").is_none());
}

#[test]
fn test_openapi_json_has_stable_object_order() {
    assert_json_objects_are_sorted(&serde_json::from_str(&openapi_json()).unwrap());
}

#[test]
fn test_none_openapi_omits_distributed_routes() {
    let spec = serde_json::to_value(openapi_for(peryx_ha::AvailabilityResources::None)).unwrap();
    let paths = spec["paths"].as_object().unwrap();

    assert!(!paths.contains_key("/+analytics/completeness"));
    assert!(!paths.keys().any(|path| path.starts_with("/+availability/")));
}

#[test]
fn test_none_openapi_json_matches_the_none_document() {
    let json = openapi_json_for(peryx_ha::AvailabilityResources::None);

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap(),
        serde_json::to_value(openapi_for(peryx_ha::AvailabilityResources::None)).unwrap()
    );
    assert!(json.ends_with('\n'));
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

#[cfg(feature = "composition-pypi")]
fn seed_shadow_candidate(state: &peryx_driver::AppState) {
    let filename = "acme_pkg-1.0-py3-none-any.whl";
    let uploaded = peryx_ecosystem_pypi::upload::Uploaded {
        version: "1.0".to_owned(),
        file: peryx_ecosystem_pypi::File {
            filename: filename.to_owned(),
            url: format!("https://files.invalid/{filename}"),
            hashes: [("sha256".to_owned(), "1".repeat(64))].into(),
            requires_python: None,
            size: Some(1),
            upload_time: Some("2020-01-01T00:00:00Z".to_owned()),
            yanked: peryx_ecosystem_pypi::Yanked::No,
            core_metadata: peryx_ecosystem_pypi::CoreMetadata::Absent,
            dist_info_metadata: peryx_ecosystem_pypi::CoreMetadata::Absent,
            gpg_sig: None,
            provenance: peryx_ecosystem_pypi::Provenance::Absent,
        },
        trashed: None,
    };
    state
        .serving
        .meta
        .put_upload("hosted", "acme-pkg", filename, &serde_json::to_vec(&uploaded).unwrap())
        .unwrap();
    state
        .serving
        .meta
        .put_project("hosted", "acme-pkg", "acme-pkg")
        .unwrap();
}

fn openapi_parameter<'a>(spec: &'a serde_json::Value, path: &str, method: &str, name: &str) -> &'a serde_json::Value {
    spec["paths"][path][method]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == name)
        .unwrap()
}
