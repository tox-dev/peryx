//! Server-level operations: discovery, search, status, stats, metrics, and this document.

use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::{PathsBuilder, ResponseBuilder};

use super::shared::{api_json_response, query_param, route_param, text_response};

pub(super) fn service_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+status",
            PathItemBuilder::new().operation(HttpMethod::Get, status()).build(),
        )
        .path(
            "/+api",
            PathItemBuilder::new().operation(HttpMethod::Get, discovery()).build(),
        )
        .path(
            "/+search",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, package_search(false))
                .build(),
        )
        .path(
            "/+stats",
            PathItemBuilder::new().operation(HttpMethod::Get, stats()).build(),
        )
        .path(
            "/metrics",
            PathItemBuilder::new().operation(HttpMethod::Get, metrics()).build(),
        )
        .path(
            "/api-docs/openapi.json",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, openapi_endpoint())
                .build(),
        )
}

pub(super) fn package_search(scoped: bool) -> OperationBuilder {
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

pub(super) fn index_discovery() -> OperationBuilder {
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
                            "uv.toml": "publish-url = \"http://127.0.0.1:4433/root/pypi/\"\n\n[[index]]\nname = \"velodex\"\nurl = \"http://127.0.0.1:4433/root/pypi/simple/\"\ndefault = true\n\n[pip]\nindex-url = \"http://127.0.0.1:4433/root/pypi/simple/\"\n",
                            ".pypirc": "[distutils]\nindex-servers =\n    velodex\n\n[velodex]\nrepository = http://127.0.0.1:4433/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
                        }
                    }
                }),
            ),
        )
        .response("404", ResponseBuilder::new().description("No index at this route"))
}

fn discovery() -> OperationBuilder {
    OperationBuilder::new()
        .tag("discovery")
        .summary(Some("Discover this server"))
        .description(Some(
            "A compact server document with global URLs and one discovery entry per configured \
             index. It is built from configuration and request context, without reading package \
             indexes.",
        ))
        .response(
            "200",
            api_json_response(
                "The server discovery document",
                json!({
                    "version": "0.0.1",
                    "urls": {
                        "api": "http://127.0.0.1:4433/+api",
                        "status": "http://127.0.0.1:4433/+status",
                        "stats": "http://127.0.0.1:4433/+stats",
                        "openapi": "http://127.0.0.1:4433/api-docs/openapi.json",
                        "web": "http://127.0.0.1:4433/"
                    },
                    "indexes": []
                }),
            ),
        )
}

fn status() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Health and identity"))
        .description(Some(
            "Version, counters, and the configured indexes. Add `?details=admin` for bounded metadata \
             summaries used by the read-only admin status page.",
        ))
        .parameter(
            ParameterBuilder::new()
                .name("details")
                .parameter_in(ParameterIn::Query)
                .description(Some(
                    "Use `admin` to include observed project counts, uploaded file counts, and recent uploads.",
                ))
                .example(Some(json!("admin"))),
        )
        .response(
            "200",
            ResponseBuilder::new().description("The status document").content(
                "application/json",
                ContentBuilder::new()
                    .example(Some(json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "serial": 42,
                        "requests": 128,
                        "by_ecosystem": [
                            {"ecosystem": "pypi", "pages": 128, "downloads": 6, "bytes": 64_733_247,
                             "rejected": 0, "uploads": 4, "families": {"metadata": 37}}
                        ],
                        "metric_families": [
                            {"key": "metadata", "label": "PEP 658 metadata hits",
                             "roles": ["cached", "hosted", "virtual"]}
                        ],
                        "indexes": [
                            {"name": "pypi", "route": "pypi", "kind": "cached", "layers": [],
                             "uploads": false, "volatile_deletes": false, "upload_to": null,
                             "upstream": {"url": "https://pypi.org/simple/", "auth": {"kind": "none", "redacted": null}, "status": "configured", "offline": false},
                             "hosted": null, "project_count": 128, "upload_count": 0, "recent_uploads": []},
                            {"name": "hosted", "route": "hosted", "kind": "hosted", "layers": [],
                             "uploads": true, "volatile_deletes": true, "upload_to": null, "upstream": null,
                             "hosted": {"volatile": true, "upload_token": {"configured": true, "redacted": "<redacted>"}},
                             "project_count": 2, "upload_count": 4,
                             "recent_uploads": [{"project": "velodexpkg", "filename": "velodexpkg-1.0-py3-none-any.whl",
                                                "version": "1.0", "uploaded_at": "2026-01-01T00:00:00Z", "size": 1832}]},
                            {"name": "root/pypi", "route": "root/pypi", "kind": "virtual",
                             "layers": ["hosted", "pypi"], "uploads": true, "volatile_deletes": true,
                             "upload_to": "hosted",
                             "upstream": null, "hosted": null, "project_count": 0, "upload_count": 0,
                             "recent_uploads": []}
                        ]
                    })))
                    .build(),
            ),
        )
}

fn stats() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Usage statistics"))
        .description(Some(
            "Counters aggregated off the request path, drillable: no parameters for per-index totals, \
             `?index={route}` for one index's projects, `&project={name}` for one project's files. \
             Counters are grouped by the role that owns them: a neutral `base` group every index \
             reports, a `cached` group only a caching index fills, a `hosted` group only an upload \
             store fills, and an `ecosystem` map of the driver's own counters (PyPI's PEP 658 \
             sibling under `metadata`).",
        ))
        .parameter(
            ParameterBuilder::new()
                .name("index")
                .parameter_in(ParameterIn::Query)
                .description(Some("Drill into one index's projects"))
                .example(Some(json!("root/pypi"))),
        )
        .parameter(
            ParameterBuilder::new()
                .name("project")
                .parameter_in(ParameterIn::Query)
                .description(Some("With `index`, drill into one project's files"))
                .example(Some(json!("pandas"))),
        )
        .response(
            "200",
            ResponseBuilder::new()
                .description("The counters at the requested depth")
                .content(
                    "application/json",
                    ContentBuilder::new()
                        .example(Some(json!({
                            "root/pypi": {
                                "base": {"pages": 12, "downloads": 6, "bytes": 64_733_247, "rejected": 0},
                                "cached": {"refreshes": 2, "changed": 1, "stale_served": 0, "upstream_errors": 0},
                                "hosted": {"uploads": 0},
                                "ecosystem": {"metadata": 6}
                            }
                        })))
                        .build(),
                ),
        )
}

fn metrics() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Prometheus metrics"))
        .response(
            "200",
            text_response(
                "Prometheus text exposition",
                "text/plain; version=0.0.4",
                "# HELP velodex_requests_total Total HTTP requests served.\n\
                 # TYPE velodex_requests_total counter\n\
                 velodex_requests_total 128\n\
                 # HELP velodex_index_metadata_total PEP 658 metadata siblings served.\n\
                 # TYPE velodex_index_metadata_total counter\n\
                 velodex_index_metadata_total{index=\"root/pypi\",ecosystem=\"pypi\",role=\"virtual\"} 37\n",
            ),
        )
}

fn openapi_endpoint() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("This document"))
        .response(
            "200",
            ResponseBuilder::new()
                .description("The OpenAPI 3.1 description of this server")
                .content("application/json", ContentBuilder::new().build()),
        )
}
