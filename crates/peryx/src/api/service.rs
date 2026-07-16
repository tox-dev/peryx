//! Server-level operations: discovery, search, status, stats, metrics, and this document.

use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::{PathsBuilder, Required, ResponseBuilder, SecurityRequirement};

use peryx_driver::openapi::{api_json_response, package_search, text_response};

pub(super) fn service_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+status",
            PathItemBuilder::new().operation(HttpMethod::Get, status()).build(),
        )
        .path(
            "/+health",
            PathItemBuilder::new().operation(HttpMethod::Get, health()).build(),
        )
        .path(
            "/+ready",
            PathItemBuilder::new().operation(HttpMethod::Get, readiness()).build(),
        )
        .path(
            "/+acl",
            PathItemBuilder::new().operation(HttpMethod::Get, acl()).build(),
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

fn acl() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("An index's access control"))
        .description(Some(
            "The tokens, grants, expiry, and anonymous-read policy one index is configured with. peryx \
             has no server-wide administrator, so the gate is the index's own: authenticate with HTTP \
             Basic as a token holding write over every project here (the `upload_token` standing). Token \
             secrets are never returned, only a marker that one is set.",
        ))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .parameter(
            ParameterBuilder::new()
                .name("index")
                .parameter_in(ParameterIn::Query)
                .required(Required::True)
                .description(Some("The route of the index to describe"))
                .example(Some(json!("hosted"))),
        )
        .response(
            "200",
            api_json_response(
                "The index's tokens and read policy, secrets redacted",
                json!({
                    "index": "hosted",
                    "route": "hosted",
                    "anonymous_read": true,
                    "tokens": [
                        {"name": "upload_token", "secret": {"configured": true, "redacted": "<redacted>"},
                         "expires_at": null, "grants": [{"projects": ["*"], "actions": ["write", "delete"]}]},
                        {"name": "ci", "secret": {"configured": true, "redacted": "<redacted>"},
                         "expires_at": 1_800_000_000, "grants": [{"projects": ["team/*"], "actions": ["read"]}]}
                    ]
                }),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No credential the index accepts was presented"),
        )
        .response(
            "403",
            ResponseBuilder::new().description("The credential does not administer this index"),
        )
        .response("404", ResponseBuilder::new().description("No index has this route"))
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
                        "health": "http://127.0.0.1:4433/+health",
                        "readiness": "http://127.0.0.1:4433/+ready",
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
                        "role": "writer",
                        "health": {
                            "serving_reads": true,
                            "accepting_writes": true,
                            "metadata_store": "healthy",
                            "blob_store": "healthy",
                            "upstreams": {"reachable": 1, "unreachable": 0, "unknown": 0, "disabled": 0}
                        },
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
                             "recent_uploads": [{"project": "peryxpkg", "filename": "peryxpkg-1.0-py3-none-any.whl",
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

fn health() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Local store health"))
        .description(Some("Checks that the metadata and blob stores remain available."))
        .response(
            "200",
            ResponseBuilder::new().description("Both local stores are healthy"),
        )
        .response(
            "503",
            ResponseBuilder::new().description("The metadata or blob store is unavailable"),
        )
}

fn readiness() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Read or write readiness"))
        .description(Some(
            "Checks read readiness by default. Set `writes=true` for a probe that accepts only a healthy writer.",
        ))
        .parameter(
            ParameterBuilder::new()
                .name("writes")
                .parameter_in(ParameterIn::Query)
                .description(Some("Require the node to accept writes"))
                .example(Some(json!(true))),
        )
        .response(
            "200",
            ResponseBuilder::new().description("The requested traffic class is ready"),
        )
        .response(
            "503",
            ResponseBuilder::new().description("The requested traffic class is not ready"),
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
                "# HELP peryx_requests_total Total HTTP requests served.\n\
                 # TYPE peryx_requests_total counter\n\
                 peryx_requests_total 128\n\
                 # HELP peryx_index_metadata_total PEP 658 metadata siblings served.\n\
                 # TYPE peryx_index_metadata_total counter\n\
                 peryx_index_metadata_total{index=\"root/pypi\",ecosystem=\"pypi\",role=\"virtual\"} 37\n",
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
