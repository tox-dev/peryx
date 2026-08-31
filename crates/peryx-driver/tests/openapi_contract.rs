use std::collections::BTreeSet;

use rstest::rstest;
use serde_json::{Value, json};

use peryx_driver::openapi::{api_json_response, artifact_search, query_param, route_param, text_response};
use peryx_search::{ContentSource, SearchResult};

fn value<T: serde::Serialize>(input: T) -> Value {
    serde_json::to_value(input).unwrap()
}

#[test]
fn route_parameter_matches_contract() {
    assert_eq!(
        value(route_param().build()),
        json!({
            "name": "route",
            "in": "path",
            "description": "The index route, for example `team/catalog`",
            "required": true,
            "schema": {"type": "string"},
            "example": "team/catalog",
        }),
    );
}

#[rstest]
#[case::null(json!(null), json!({}))]
#[case::boolean(json!(true), json!({"type": "boolean"}))]
#[case::integer(json!(2), json!({"type": "integer"}))]
#[case::number(json!(2.5), json!({"type": "number"}))]
#[case::string(json!("two"), json!({"type": "string"}))]
#[case::array(json!([2]), json!({"type": "array"}))]
#[case::object(json!({"page": 2}), json!({"type": "object"}))]
fn query_parameter_matches_contract(#[case] example: Value, #[case] schema: Value) {
    assert_eq!(
        value(query_param("page", "One-based page.", example.clone()).build()),
        json!({
            "name": "page",
            "in": "query",
            "description": "One-based page.",
            "required": false,
            "schema": schema,
            "example": example,
        }),
    );
}

#[test]
fn json_response_matches_contract() {
    assert_eq!(
        value(api_json_response("Result", json!({"ok": true})).build()),
        json!({
            "description": "Result",
            "content": {
                "application/json": {
                    "example": {"ok": true},
                },
            },
        }),
    );
}

#[test]
fn text_response_matches_contract() {
    assert_eq!(
        value(text_response("Page", "text/html", "<p>ok</p>").build()),
        json!({
            "description": "Page",
            "content": {
                "text/html": {
                    "example": "<p>ok</p>",
                },
            },
        }),
    );
}

#[rstest]
#[case::global(false, "Search cached resources", false)]
#[case::scoped(true, "Search one index route", true)]
fn artifact_search_matches_contract(#[case] scoped: bool, #[case] summary: &str, #[case] includes_route: bool) {
    let mut parameters = search_parameters();
    if includes_route {
        parameters.push(route_parameter());
    }

    assert_eq!(
        value(artifact_search(scoped).build()),
        json!({
            "tags": ["search"],
            "summary": summary,
            "description": "Searches the derived artifact index built from cached listings, local writes, and cached \
                            metadata. `q` uses substring matching and needs at least two characters; prefix it with \
                            `re:` for a regex, which reads every indexed document and is restricted to operators. \
                            Index policy removes denied entries before indexing. Results are sorted by display name \
                            and paged without collecting every match.",
            "parameters": parameters,
            "responses": {
                "200": {
                    "description": "Search results",
                    "content": {
                        "application/json": {
                            "example": {
                                "query": "widget",
                                "type": "all",
                                "availability": "all",
                                "page": 1,
                                "page_size": 25,
                                "total": 1,
                                "results": [{
                                    "display_label": "Widget",
                                    "resource_key": "widget",
                                    "route": "team/catalog",
                                    "index": "team/catalog",
                                    "ecosystem": "pypi",
                                    "type_label": "project",
                                    "type": "cached",
                                    "available": true,
                                    "summary": "An indexed artifact.",
                                }],
                            },
                        },
                    },
                },
                "400": {
                    "description": "Invalid search parameters",
                    "content": {
                        "application/json": {
                            "example": {"error": "invalid resource source type"},
                        },
                    },
                },
                "403": {
                    "description": "Pattern search without operator authority",
                    "content": {
                        "application/json": {
                            "example": {"error": "pattern search requires operator authority"},
                        },
                    },
                },
            },
        }),
    );
}

#[test]
fn artifact_search_result_example_matches_serialized_contract() {
    let example =
        value(artifact_search(false).build())["responses"]["200"]["content"]["application/json"]["example"]["results"]
            [0]
        .clone();
    let expected = SearchResult {
        display_label: "Widget".into(),
        resource_key: "widget".into(),
        route: "team/catalog".into(),
        index: "team/catalog".into(),
        ecosystem: "pypi".into(),
        type_label: "project".into(),
        source_type: ContentSource::Cached,
        available_locally: true,
        summary: Some("An indexed artifact.".into()),
    };

    assert_eq!(
        example.as_object().unwrap().keys().collect::<BTreeSet<_>>(),
        value(&expected).as_object().unwrap().keys().collect::<BTreeSet<_>>(),
    );
    assert_eq!(serde_json::from_value::<SearchResult>(example).unwrap(), expected);
}

fn search_parameters() -> Vec<Value> {
    vec![
        json!({
            "name": "q",
            "in": "query",
            "description": "Search text of at least two characters. Prefix with `re:` to use a regex, which operators \
                            alone may run.",
            "required": false,
            "schema": {"type": "string"},
            "example": "widget",
        }),
        json!({
            "name": "type",
            "in": "query",
            "description": "`uploaded`, `cached`, or `override`; omit for all sources.",
            "required": false,
            "schema": {"type": "string", "enum": ["uploaded", "cached", "override"]},
            "example": "override",
        }),
        json!({
            "name": "availability",
            "in": "query",
            "description": "`local` returns only resources whose bytes are available from local storage now; omit or \
                            `all` returns every indexed resource.",
            "required": false,
            "schema": {"type": "string", "enum": ["local", "all"]},
            "example": "local",
        }),
        json!({
            "name": "page",
            "in": "query",
            "description": "One-based page number.",
            "required": false,
            "schema": {"type": "integer", "minimum": 1},
            "example": 1,
        }),
        json!({
            "name": "page_size",
            "in": "query",
            "description": "Page size: 25, 50, or 100.",
            "required": false,
            "schema": {"type": "integer", "enum": [25, 50, 100]},
            "example": 25,
        }),
    ]
}

fn route_parameter() -> Value {
    json!({
        "name": "route",
        "in": "path",
        "description": "The index route, for example `team/catalog`",
        "required": true,
        "schema": {"type": "string"},
        "example": "team/catalog",
    })
}
