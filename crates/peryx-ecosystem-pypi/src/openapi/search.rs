use super::shared::{OperationBuilder, api_json_response, json, route_param};

pub(super) fn package_search() -> OperationBuilder {
    OperationBuilder::new()
        .tag("search")
        .summary(Some("Search one PyPI index route"))
        .description(Some(
            "Searches PyPI projects derived from cached listings, uploads, and metadata. `q` uses substring matching; \
             prefix it with `re:` for a regex. Policy-denied projects are not indexed.",
        ))
        .parameter(peryx_driver::openapi::query_param(
            "q",
            "Search text. Prefix with `re:` to use a regex.",
            json!("widget"),
        ))
        .parameter(peryx_driver::openapi::query_param(
            "type",
            "`uploaded`, `cached`, or `override`; omit for all sources.",
            json!("override"),
        ))
        .parameter(peryx_driver::openapi::query_param(
            "availability",
            "`local` returns projects with locally available files; omit or use `all` for every indexed project.",
            json!("local"),
        ))
        .parameter(peryx_driver::openapi::query_param(
            "page",
            "One-based page number.",
            json!(1),
        ))
        .parameter(peryx_driver::openapi::query_param(
            "page_size",
            "Page size: 25, 50, or 100.",
            json!(25),
        ))
        .parameter(route_param())
        .response(
            "200",
            api_json_response(
                "Search results",
                json!({
                    "query": "widget",
                    "type": "all",
                    "availability": "all",
                    "page": 1,
                    "page_size": 25,
                    "total": 1,
                    "results": [{
                        "display_name": "Widget",
                        "normalized_name": "widget",
                        "route": "team/packages",
                        "index": "team/packages",
                        "type": "cached",
                        "available": true,
                        "summary": "A Python package."
                    }]
                }),
            ),
        )
        .response(
            "400",
            api_json_response(
                "Invalid search parameters",
                json!({"error": "invalid package source type"}),
            ),
        )
}
