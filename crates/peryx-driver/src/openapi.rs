//! The `OpenAPI` builders every ecosystem's operations share, plus peryx's own per-index endpoints.
//!
//! An ecosystem crate describes its own wire protocol with these. The two operations here — `+api`
//! and `+search` — are peryx's own, served for every index whatever ecosystem it speaks, so each
//! driver mounts them under its route rather than redescribing them.

use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{OperationBuilder, ParameterBuilder, ParameterIn};
use utoipa::openapi::{Required, ResponseBuilder};

#[must_use]
pub fn route_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("route")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The index route, for example `root/pypi`"))
        .example(Some(json!("root/pypi")))
}

#[must_use]
pub fn query_param(name: &'static str, description: &'static str, example: serde_json::Value) -> ParameterBuilder {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(ParameterIn::Query)
        .description(Some(description))
        .example(Some(example))
}

#[must_use]
pub fn api_json_response(description: &str, example: serde_json::Value) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content("application/json", ContentBuilder::new().example(Some(example)).build())
}

#[must_use]
pub fn text_response(description: &str, content_type: &str, example: &str) -> ResponseBuilder {
    ResponseBuilder::new().description(description).content(
        content_type,
        ContentBuilder::new().example(Some(json!(example))).build(),
    )
}

#[must_use]
pub fn package_search(scoped: bool) -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("search")
        .summary(Some(if scoped {
            "Search one index route"
        } else {
            "Search cached packages"
        }))
        .description(Some(
            "Searches the derived package index built from cached simple pages, local uploads, \
             and cached core metadata. `q` uses substring matching; prefix it with `re:` for a \
             regex. Index policy removes denied packages before indexing. Results are sorted \
             by display name and paged without collecting every match.",
        ))
        .parameter(query_param(
            "q",
            "Search text. Prefix with `re:` to use a regex.",
            json!("flask"),
        ))
        .parameter(query_param(
            "type",
            "`uploaded`, `cached`, or `override`; omit for all sources.",
            json!("override"),
        ))
        .parameter(query_param("page", "One-based page number.", json!(1)))
        .parameter(query_param("page_size", "Page size: 25, 50, or 100.", json!(25)))
        .response(
            "200",
            api_json_response(
                "Search results",
                json!({
                    "query": "flask",
                    "type": "all",
                    "page": 1,
                    "page_size": 25,
                    "total": 1,
                    "results": [{
                        "display_name": "Flask",
                        "normalized_name": "flask",
                        "route": "root/pypi",
                        "index": "root/pypi",
                        "type": "cached",
                        "summary": "A simple framework for building complex web applications.",
                    }],
                }),
            ),
        )
        .response(
            "400",
            api_json_response(
                "Invalid search parameters",
                json!({"error": "invalid package source type"}),
            ),
        );
    if scoped {
        operation = operation.parameter(route_param());
    }
    operation
}

#[must_use]
pub fn index_discovery() -> OperationBuilder {
    OperationBuilder::new()
        .tag("discovery")
        .summary(Some("Discover one index"))
        .description(Some(
            "A compact index document with URLs and client configuration snippets. Snippets appear \
             only when the request has enough host context to render absolute URLs.",
        ))
        .parameter(route_param())
        .response(
            "200",
            api_json_response(
                "The index discovery document",
                json!({
                    "version": "0.0.1",
                    "index": {
                        "name": "root/pypi",
                        "route": "root/pypi",
                        "kind": "virtual",
                        "layers": ["hosted", "pypi"],
                        "uploads": true,
                        "upload_to": "hosted",
                        "capabilities": {
                            "simple_html": true,
                            "simple_json": true,
                            "simple_api_version": "1.4",
                            "metadata_siblings": true,
                            "uploads": true,
                            "yanking": true,
                            "volatile_deletes": true,
                            "project_status": true,
                            "provenance": true,
                            "legacy_json": true
                        },
                        "urls": {
                            "api": "http://127.0.0.1:4433/root/pypi/+api",
                            "simple": "http://127.0.0.1:4433/root/pypi/simple/",
                            "upload": "http://127.0.0.1:4433/root/pypi/",
                            "status": "http://127.0.0.1:4433/+status",
                            "web": "http://127.0.0.1:4433/browse?index=root%2Fpypi",
                            "stats": "http://127.0.0.1:4433/stats?index=root%2Fpypi",
                            "openapi": "http://127.0.0.1:4433/api-docs/openapi.json"
                        },
                        "client_configuration": {
                            "pip.conf": "[global]\nindex-url = http://127.0.0.1:4433/root/pypi/simple/\n",
                            "uv.toml": "publish-url = \"http://127.0.0.1:4433/root/pypi/\"\n\n[[index]]\nname = \"peryx\"\nurl = \"http://127.0.0.1:4433/root/pypi/simple/\"\ndefault = true\n\n[pip]\nindex-url = \"http://127.0.0.1:4433/root/pypi/simple/\"\n",
                            ".pypirc": "[distutils]\nindex-servers =\n    peryx\n\n[peryx]\nrepository = http://127.0.0.1:4433/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
                        }
                    }
                }),
            ),
        )
        .response("404", ResponseBuilder::new().description("No index at this route"))
}
