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
    let plugin_spec = serde_json::to_value(
        crate::compiled_plugins()
            .openapi_paths(PathsBuilder::new(), peryx_driver::route_auth::ReadExposure::Protected)
            .build(),
    )
    .unwrap();
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
                    .map(move |parameter| {
                        let shapes = ["schema", "content"]
                            .into_iter()
                            .filter(|field| parameter.get(field).is_some())
                            .count();
                        (method, path, &parameter["in"], &parameter["name"], shapes)
                    })
            })
        })
        .filter(|(.., shapes)| *shapes != 1)
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

/// Every name an operation requires is a scheme the components section declares, and every declared
/// scheme is required somewhere. A contract that names a credential it does not declare, or declares
/// one no route takes, describes a client that cannot authenticate.
#[test]
fn test_every_required_security_scheme_is_declared_and_used() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let declared: BTreeSet<&str> = spec["components"]["securitySchemes"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let required: BTreeSet<&str> = spec["paths"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|methods| methods.as_object().unwrap().values())
        .filter_map(|operation| operation.get("security"))
        .flat_map(|requirements| requirements.as_array().unwrap())
        .flat_map(|requirement| requirement.as_object().unwrap().keys())
        .map(String::as_str)
        .collect();

    assert_eq!(declared, required);
}

/// A read-only credential must not imply writes, so a protected read names neither the write-granting
/// scheme nor the alias it used to carry.
#[test]
#[cfg(feature = "composition-pypi")]
fn test_protected_reads_do_not_require_the_write_scheme() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let pull = &spec["paths"]["/{route}/simple/{project}/"]["get"]["security"];

    assert_eq!(
        *pull,
        serde_json::json!([{"indexAccessToken": []}, {"bearerGrant": []}])
    );
}

/// Every operation names a tag and a summary, answers with at least one described response, and every
/// JSON body it describes carries an example or a schema. Checked document-wide rather than per route,
/// so a builder that quietly returns an empty operation, example, or request body fails here regardless
/// of which route it serves. Binary bodies are exempt: an octet stream has no example worth printing.
#[test]
fn test_every_operation_is_fully_described() {
    let spec = serde_json::to_value(openapi()).unwrap();

    let mut undescribed = Vec::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            let mut missing = Vec::new();
            if operation["tags"].as_array().is_none_or(Vec::is_empty) {
                missing.push("tags".to_owned());
            }
            if operation["summary"].as_str().is_none_or(str::is_empty) {
                missing.push("summary".to_owned());
            }
            let responses = operation["responses"].as_object().unwrap();
            if responses.is_empty() {
                missing.push("responses".to_owned());
            }
            for (status, response) in responses {
                if response["description"].as_str().is_none_or(str::is_empty) {
                    missing.push(format!("{status} description"));
                }
                missing.extend(undescribed_content(&response["content"], status));
            }
            if let Some(body) = operation.get("requestBody") {
                if body["content"].as_object().is_none_or(serde_json::Map::is_empty) {
                    missing.push("requestBody content".to_owned());
                }
                missing.extend(undescribed_content(&body["content"], "requestBody"));
            }
            if !missing.is_empty() {
                undescribed.push((path.clone(), method.clone(), missing));
            }
        }
    }

    assert_eq!(undescribed, Vec::new());
}

fn undescribed_content(content: &serde_json::Value, owner: &str) -> Vec<String> {
    content
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(media_type, media)| {
            media_type.contains("json") && media["example"].is_null() && media["schema"].is_null()
        })
        .map(|(media_type, _)| format!("{owner} {media_type} example"))
        .collect()
}

/// The trash document uses the handler's wire names: the record examples carry exactly the fields it
/// writes, and the inspect query names the parameters it reads, so a reader building against the
/// document does not send `name` for `resource` or `reference` for `artifact`.
#[test]
fn test_trash_document_uses_the_wire_names() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let example =
        |path: &str| spec["paths"][path]["get"]["responses"]["200"]["content"]["application/json"]["example"].clone();
    let expected = BTreeSet::from([
        "actor",
        "artifact",
        "deadline_unix",
        "deleted_at_unix",
        "digest",
        "ecosystem",
        "reason",
        "repository",
        "resource",
        "restorable",
        "state",
    ]);

    for record in [
        example("/+trash")["trash"][0].clone(),
        example("/+trash/record")["record"].clone(),
    ] {
        let keys: BTreeSet<&str> = record.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, expected);
    }
    let query: BTreeSet<&str> = spec["paths"]["/+trash/record"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|parameter| parameter["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        query,
        BTreeSet::from(["artifact", "digest", "ecosystem", "repository", "resource"])
    );
}

/// The nested objects the analytics, quota and retention examples share are real records, not
/// placeholders: every analytics view shows the same resolved window, every quota meter carries all
/// four counters, and a retention candidate names what the plan decided about it.
#[test]
fn test_shared_example_records_carry_their_fields() {
    let spec = serde_json::to_value(openapi()).unwrap();
    let example = |path: &str, method: &str| {
        spec["paths"][path][method]["responses"]["200"]["content"]["application/json"]["example"].clone()
    };
    let keys = |value: &serde_json::Value| -> BTreeSet<String> { value.as_object().unwrap().keys().cloned().collect() };
    let interval = BTreeSet::from(
        [
            "from_day",
            "to_day",
            "from_unix",
            "to_unix",
            "retained_from_day",
            "window_clamped_to_retention",
        ]
        .map(str::to_owned),
    );
    let meter = BTreeSet::from(["committed", "reserved", "limit", "remaining"].map(str::to_owned));

    for view in ["top-resources", "unused", "groups", "sources", "timeline"] {
        assert_eq!(
            keys(&example(&format!("/+analytics/{view}"), "get")["interval"]),
            interval,
            "{view}"
        );
    }
    let quota = example("/+quota/repository", "get");
    for counter in ["artifact_bytes", "accounted_bytes", "resources"] {
        assert_eq!(keys(&quota[counter]), meter, "{counter}");
    }
    let candidate = BTreeSet::from(
        [
            "resource",
            "group",
            "artifact",
            "digest",
            "class",
            "visibility",
            "bytes",
            "outcome",
            "rule",
            "retained_groups",
        ]
        .map(str::to_owned),
    );
    assert_eq!(keys(&example("/+retention/plan", "post")["candidates"][0]), candidate);
}
