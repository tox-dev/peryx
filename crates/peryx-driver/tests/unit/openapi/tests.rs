use serde_json::{Value, json};

use super::{api_json_response, package_search, query_param, route_param, text_response};

fn value<T: serde::Serialize>(input: T) -> Value {
    serde_json::to_value(input).unwrap()
}

#[test]
fn test_route_parameter_is_required() {
    let parameter = value(route_param().build());

    assert_eq!(parameter["name"], "route");
    assert_eq!(parameter["in"], "path");
    assert_eq!(parameter["required"], true);
    assert_eq!(parameter["example"], "team/packages");
}

#[test]
fn test_query_parameter_keeps_contract() {
    let parameter = value(query_param("page", "One-based page.", json!(2)).build());

    assert_eq!(parameter["name"], "page");
    assert_eq!(parameter["in"], "query");
    assert_eq!(parameter["description"], "One-based page.");
    assert_eq!(parameter["example"], 2);
}

#[test]
fn test_json_response_keeps_media_type_and_example() {
    let response = value(api_json_response("Result", json!({"ok": true})).build());

    assert_eq!(response["description"], "Result");
    assert_eq!(response["content"]["application/json"]["example"]["ok"], true);
}

#[test]
fn test_text_response_keeps_media_type_and_example() {
    let response = value(text_response("Page", "text/html", "<p>ok</p>").build());

    assert_eq!(response["description"], "Page");
    assert_eq!(response["content"]["text/html"]["example"], "<p>ok</p>");
}

#[test]
fn test_global_package_search_omits_route_parameter() {
    let operation = value(package_search(false).build());

    assert_eq!(operation["summary"], "Search cached packages");
    assert!(
        operation["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .all(|parameter| parameter["name"] != "route")
    );
    assert!(operation["responses"].get("200").is_some());
    assert!(operation["responses"].get("400").is_some());
}

#[test]
fn test_scoped_package_search_requires_route_parameter() {
    let operation = value(package_search(true).build());

    assert_eq!(operation["summary"], "Search one index route");
    assert!(
        operation["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|parameter| parameter["name"] == "route")
    );
}
