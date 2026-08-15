use rstest::rstest;
use serde_json::{Value, json};

use peryx_driver::openapi::{api_json_response, artifact_search, query_param, route_param, text_response};

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
            "example": "team/catalog",
        }),
    );
}

#[test]
fn query_parameter_matches_contract() {
    assert_eq!(
        value(query_param("page", "One-based page.", json!(2)).build()),
        json!({
            "name": "page",
            "in": "query",
            "description": "One-based page.",
            "required": false,
            "example": 2,
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
                            metadata. `q` uses substring matching; prefix it with `re:` for a regex. Index policy \
                            removes denied entries before indexing. Results are sorted by display name and paged \
                            without collecting every match.",
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
                                    "display_name": "Widget",
                                    "normalized_name": "widget",
                                    "route": "team/catalog",
                                    "index": "team/catalog",
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
            },
        }),
    );
}

fn search_parameters() -> Vec<Value> {
    vec![
        json!({
            "name": "q",
            "in": "query",
            "description": "Search text. Prefix with `re:` to use a regex.",
            "required": false,
            "example": "widget",
        }),
        json!({
            "name": "type",
            "in": "query",
            "description": "`uploaded`, `cached`, or `override`; omit for all sources.",
            "required": false,
            "example": "override",
        }),
        json!({
            "name": "availability",
            "in": "query",
            "description": "`local` returns only resources whose bytes are available from local storage now; omit or \
                            `all` returns every indexed resource.",
            "required": false,
            "example": "local",
        }),
        json!({
            "name": "page",
            "in": "query",
            "description": "One-based page number.",
            "required": false,
            "example": 1,
        }),
        json!({
            "name": "page_size",
            "in": "query",
            "description": "Page size: 25, 50, or 100.",
            "required": false,
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
        "example": "team/catalog",
    })
}
