//! Server-level operations: discovery, search, status, stats, metrics, and this document.

use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::request_body::RequestBodyBuilder;
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
            "/+analytics/top-packages",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, top_packages())
                .build(),
        )
        .path(
            "/+policy/decisions",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, policy_decisions())
                .build(),
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
        .path(
            "/_/oidc/audience",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oidc_audience())
                .build(),
        )
        .path(
            "/_/oidc/mint-token",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, oidc_mint_token())
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

fn top_packages() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Most-downloaded packages"))
        .description(Some(
            "Durable download counts and bytes grouped by repository and project. Results are ordered by \
             downloads, bytes, repository, then project.",
        ))
        .parameter(
            ParameterBuilder::new()
                .name("limit")
                .parameter_in(ParameterIn::Query)
                .description(Some("Number of projects to return, from 1 through 100; defaults to 25"))
                .example(Some(json!(10))),
        )
        .response(
            "200",
            api_json_response(
                "The highest-usage projects",
                json!([
                    {"repository": "pypi", "project": "pandas", "downloads": 42, "bytes": 64_733_247}
                ]),
            ),
        )
        .response(
            "400",
            api_json_response(
                "The limit is outside the accepted range",
                json!({"error": "limit must be between 1 and 100"}),
            ),
        )
}

fn policy_decisions() -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("operations")
        .summary(Some("Repository policy decisions"))
        .description(Some(
            "Bounded policy decision history for one repository. The repository's administering token is required; \
             records contain package subjects and matched rule IDs without credentials or request headers. `fresh` is \
             false after repository data, catalog, or policy inputs change.",
        ))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .response(
            "200",
            api_json_response(
                "The matching decisions, newest first",
                json!({
                    "decisions": [{
                        "id": "550e8400-e29b-41d4-a716-446655440000",
                        "repository": "private",
                        "project": "example",
                        "version": "1.0",
                        "filename": "example-1.0-py3-none-any.whl",
                        "source": "pypi",
                        "action": "serve",
                        "state": "deny",
                        "rule": "blocked-project",
                        "reason": "project is blocked",
                        "evaluated_at_unix": 1_800_000_000,
                        "input_generation": {"repository": 42, "catalog": 7, "policy": 3},
                        "next_eligible_at_unix": null,
                        "fresh": true
                    }],
                    "next_cursor": "pd_000000000000002a"
                }),
            ),
        )
        .response(
            "400",
            api_json_response(
                "The limit or cursor is invalid",
                json!({"error": "limit must be between 1 and 100"}),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No credential the repository accepts was presented"),
        )
        .response(
            "403",
            ResponseBuilder::new().description("The credential does not administer this repository"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("No repository has this route"),
        )
        .response(
            "500",
            api_json_response(
                "The decision store could not complete the query",
                json!({"error": "policy decision query failed"}),
            ),
        );
    for (name, description, example) in [
        ("repository", "Repository route to inspect", json!("private")),
        ("state", "Filter by `allow`, `deny`, or `wait`", json!("deny")),
        ("rule", "Filter by matched rule ID", json!("blocked-project")),
        ("source", "Filter by routed source", json!("pypi")),
        ("from", "Minimum evaluation Unix timestamp", json!(1_700_000_000)),
        ("to", "Maximum evaluation Unix timestamp", json!(1_800_000_000)),
        (
            "cursor",
            "Exclusive cursor from the prior page",
            json!("pd_000000000000002a"),
        ),
        ("limit", "Rows to return, from 1 through 100; defaults to 25", json!(25)),
    ] {
        let mut parameter = ParameterBuilder::new()
            .name(name)
            .parameter_in(ParameterIn::Query)
            .description(Some(description))
            .example(Some(example));
        if name == "repository" {
            parameter = parameter.required(Required::True);
        }
        operation = operation.parameter(parameter);
    }
    operation
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
                 # HELP peryx_metadata_served_total PEP 658 metadata siblings served.\n\
                 # TYPE peryx_metadata_served_total counter\n\
                 peryx_metadata_served_total{ecosystem=\"pypi\",role=\"virtual\"} 37\n",
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

fn oidc_audience() -> OperationBuilder {
    OperationBuilder::new()
        .tag("trusted publishing")
        .summary(Some("Discover the CI identity audience"))
        .description(Some(
            "Peryx returns the audience a CI provider must put in its OIDC identity. Peryx adds this route after an operator configures a trusted publisher.",
        ))
        .response(
            "200",
            api_json_response(
                "The configured OIDC audience",
                json!({"audience": "packages.example"}),
            ),
        )
        .response("404", ResponseBuilder::new().description("No trusted publisher exists"))
}

fn oidc_mint_token() -> OperationBuilder {
    OperationBuilder::new()
        .tag("trusted publishing")
        .summary(Some("Exchange a CI identity for an upload token"))
        .description(Some(
            "Peryx verifies one external OIDC identity against a configured publisher and returns a short-lived token restricted to that publisher's repository and projects.",
        ))
        .request_body(Some(
            RequestBodyBuilder::new()
                .required(Some(Required::True))
                .content(
                    "application/json",
                    ContentBuilder::new()
                        .example(Some(json!({"token": "eyJhbGciOiJSUzI1NiIs..."})))
                        .build(),
                )
                .build(),
        ))
        .response(
            "200",
            api_json_response(
                "A repository- and project-scoped upload token",
                json!({"token": "eyJhbGciOiJIUzI1NiIs...", "expires": 1_800_000_000_i64}),
            ),
        )
        .response("404", ResponseBuilder::new().description("No trusted publisher exists"))
        .response(
            "413",
            ResponseBuilder::new().description("The exchange request exceeds the fixed body limit"),
        )
        .response(
            "422",
            api_json_response(
                "The external identity is invalid or unauthorized",
                json!({"message": "identity token rejected"}),
            ),
        )
        .response(
            "503",
            api_json_response(
                "The identity provider or replay guard is unavailable",
                json!({"message": "identity provider unavailable"}),
            ),
        )
}
