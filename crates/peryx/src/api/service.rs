use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::{PathsBuilder, Required, ResponseBuilder, SecurityRequirement};

use peryx_driver::openapi::{api_json_response, artifact_search, text_response};

/// Register the `/+analytics/*` usage query family, kept apart so the service path list stays short.
fn analytics_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+analytics/top-resources",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, analytics_top())
                .build(),
        )
        .path(
            "/+analytics/unused",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, analytics_unused())
                .build(),
        )
        .path(
            "/+analytics/groups",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, analytics_groups())
                .build(),
        )
        .path(
            "/+analytics/sources",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, analytics_sources())
                .build(),
        )
        .path(
            "/+analytics/timeline",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, analytics_timeline())
                .build(),
        )
}

/// Register the `/+quota` read family, kept apart so the service path list stays short.
fn quota_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+quota",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, quota_summary())
                .build(),
        )
        .path(
            "/+quota/repository",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, quota_repository())
                .build(),
        )
}

/// Register the `/+grants` role-grant family, kept apart so the service path list stays short.
fn repository_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+repositories",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, list_repositories())
                .operation(HttpMethod::Post, create_repository())
                .build(),
        )
        .path(
            "/+repositories/{id}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, inspect_repository())
                .operation(HttpMethod::Put, update_repository())
                .build(),
        )
        .path(
            "/+repositories/{id}/disable",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, repository_state_operation("Disable a repository", "Disables a repository, conditional on an `If-Match` version. Idempotent: disabling an already-disabled repository returns it unchanged."))
                .build(),
        )
        .path(
            "/+repositories/{id}/enable",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, repository_state_operation("Enable a repository", "Re-enables a disabled repository, conditional on an `If-Match` version."))
                .build(),
        )
}

fn repository_example() -> serde_json::Value {
    json!({
        "id": "repo_2f7e6a1b9c4d4e2f8a1b2c3d4e5f6a7b", "route": "root/artifacts", "display_name": "Artifact mirror",
        "ecosystem": "example", "definition": {}, "state": "enabled", "version": 1,
        "created_by": "usr_550e8400e29b41d4a716446655440000", "created_at_unix": 1_700_000_000,
        "updated_by": "usr_550e8400e29b41d4a716446655440000", "updated_at_unix": 1_700_000_000
    })
}

fn repository_id_parameter() -> utoipa::openapi::path::Parameter {
    ParameterBuilder::new()
        .name("id")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some(
            "Opaque, stable repository identifier from a create or list response",
        ))
        .example(Some(json!("repo_2f7e6a1b9c4d4e2f8a1b2c3d4e5f6a7b")))
        .build()
}

fn list_repositories() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("List repositories"))
            .description(Some(
                "Lists repositories in id order with an opaque cursor and a bounded page size. Filter by state.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .parameter(
                ParameterBuilder::new()
                    .name("state")
                    .parameter_in(ParameterIn::Query)
                    .required(Required::False)
                    .description(Some("Filter by `enabled` or `disabled`"))
                    .build(),
            )
            .parameter(
                ParameterBuilder::new()
                    .name("cursor")
                    .parameter_in(ParameterIn::Query)
                    .required(Required::False)
                    .description(Some("Opaque cursor from a prior page's `next_cursor`"))
                    .build(),
            )
            .parameter(
                ParameterBuilder::new()
                    .name("limit")
                    .parameter_in(ParameterIn::Query)
                    .required(Required::False)
                    .description(Some("Page size, 1..=100 (default 25)"))
                    .build(),
            )
            .response(
                "200",
                api_json_response(
                    "A page of repositories",
                    json!({"repositories": [repository_example()], "next_cursor": serde_json::Value::Null}),
                ),
            )
            .response(
                "400",
                api_json_response(
                    "The limit is out of range",
                    json!({"error": "limit must be between 1 and 100"}),
                ),
            ),
    )
}

fn create_repository() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Create a repository"))
            .description(Some(
                "Creates a repository under a unique route, minting a stable id. The route and ecosystem are fixed \
                 for the record's life. The response carries the record, an `ETag` for a later `If-Match`, and a \
                 `Location`.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .request_body(Some(RequestBodyBuilder::new().required(Some(Required::True)).content("application/json", ContentBuilder::new().example(Some(json!({"route": "root/artifacts", "display_name": "Artifact mirror", "ecosystem": "example", "definition": {}}))).build()).build()))
            .response("201", api_json_response("The created repository", repository_example()))
            .response("409", api_json_response("Another repository already serves the route", json!({"error": "a repository already serves route \"root/artifacts\""})))
            .response("415", ResponseBuilder::new().description("The request is not JSON"))
            .response("422", api_json_response("The body is malformed or a field is invalid", json!({"error": "route must not be empty"}))),
    )
}

fn inspect_repository() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Inspect a repository"))
            .description(Some(
                "Returns one repository by id, with an `ETag` carrying its version.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .parameter(repository_id_parameter())
            .response("200", api_json_response("The repository", repository_example())),
    )
}

fn update_repository() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Update a repository"))
            .description(Some(
                "Replaces a repository's display name and definition, conditional on an `If-Match` version. The \
                 route and ecosystem cannot change. A stale precondition conflicts and the winning version rides \
                 back on the `ETag`.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .parameter(repository_id_parameter())
            .request_body(Some(
                RequestBodyBuilder::new()
                    .required(Some(Required::True))
                    .content(
                        "application/json",
                        ContentBuilder::new()
                            .example(Some(json!({"display_name": "Artifact mirror", "definition": {}})))
                            .build(),
                    )
                    .build(),
            ))
            .response("200", api_json_response("The updated repository", repository_example()))
            .response(
                "400",
                api_json_response(
                    "The `If-Match` precondition is malformed",
                    json!({"error": "If-Match must be a repository version"}),
                ),
            )
            .response(
                "409",
                api_json_response(
                    "The repository is at a different version than the precondition named",
                    json!({"error": "repository version precondition failed"}),
                ),
            )
            .response("415", ResponseBuilder::new().description("The request is not JSON"))
            .response(
                "422",
                api_json_response(
                    "The body is malformed or a field is invalid",
                    json!({"error": "display name must not be empty"}),
                ),
            )
            .response(
                "428",
                api_json_response(
                    "The request carried no `If-Match`",
                    json!({"error": "an If-Match version is required"}),
                ),
            ),
    )
}

fn repository_state_operation(summary: &'static str, description: &'static str) -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some(summary))
            .description(Some(description))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .parameter(repository_id_parameter())
            .response(
                "200",
                api_json_response("The repository in its new state", repository_example()),
            )
            .response(
                "400",
                api_json_response(
                    "The `If-Match` precondition is malformed",
                    json!({"error": "If-Match must be a repository version"}),
                ),
            )
            .response(
                "409",
                api_json_response(
                    "The repository is at a different version than the precondition named",
                    json!({"error": "repository version precondition failed"}),
                ),
            )
            .response(
                "428",
                api_json_response(
                    "The request carried no `If-Match`",
                    json!({"error": "an If-Match version is required"}),
                ),
            ),
    )
}

fn grant_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+grants",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, list_grants())
                .operation(HttpMethod::Post, create_grant())
                .build(),
        )
        .path(
            "/+grants/{id}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, inspect_grant())
                .operation(HttpMethod::Delete, revoke_grant())
                .build(),
        )
}

/// Register the `/+query` read-only PQL endpoint, kept apart so the service path list stays short.
fn pql_paths(paths: PathsBuilder) -> PathsBuilder {
    paths.path(
        "/+query",
        PathItemBuilder::new().operation(HttpMethod::Post, pql_query()).build(),
    )
}

pub(super) fn service_paths(paths: PathsBuilder) -> PathsBuilder {
    let paths = grant_paths(quota_paths(analytics_paths(pql_paths(paths))));
    let paths = paths
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
                .operation(HttpMethod::Get, artifact_search(false))
                .build(),
        )
        .path(
            "/+stats",
            PathItemBuilder::new().operation(HttpMethod::Get, stats()).build(),
        )
        .path(
            "/+policy/decisions",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, policy_decisions())
                .build(),
        )
        .path(
            "/+retention/plan",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, retention_plan())
                .build(),
        )
        .path(
            "/+retention/export",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, retention_export())
                .build(),
        )
        .path(
            "/+revocations",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, list_revocations())
                .build(),
        )
        .path(
            "/+revocations/{digest}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, inspect_revocation())
                .operation(HttpMethod::Put, put_revocation())
                .build(),
        )
        .path(
            "/+revocations/{digest}/lift",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, lift_revocation())
                .build(),
        );
    let paths = repository_paths(paths);
    token_paths(paths)
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

/// Register the scoped-token lifecycle paths, kept apart so the service path list stays short.
fn token_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+tokens",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, create_token())
                .operation(HttpMethod::Get, list_tokens())
                .build(),
        )
        .path(
            "/+tokens/{id}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, inspect_token())
                .operation(HttpMethod::Delete, revoke_token())
                .build(),
        )
        .path(
            "/+tokens/{id}/rotate",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, rotate_token())
                .build(),
        )
        .path(
            "/+jobs/{id}/cancel",
            PathItemBuilder::new().operation(HttpMethod::Post, cancel_job()).build(),
        )
}

fn cancel_job() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Cancel a running job"))
        .description(Some(
            "Signals a node-local job run to stop, reaching the cooperative cancellation token that lives only in \
             the process running it, so no separate CLI process can. The run observes the signal and unwinds \
             within its grace period, so a delivered signal answers 202 rather than a completed stop. Requires the \
             administration-write scope; an unknown run and an unauthorized caller answer 404 alike, so a denial \
             cannot confirm a run id.",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .parameter(job_id_parameter())
        .response(
            "202",
            ResponseBuilder::new().description("The cancellation signal reached the running attempt"),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid local user credential was presented"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The caller cannot administer jobs, or no run has this id"),
        )
        .response(
            "409",
            api_json_response(
                "The run is not one this node is currently running",
                json!({"error": "job run is not running on this node"}),
            ),
        )
        .response(
            "503",
            api_json_response(
                "Authentication or authorization storage is unavailable",
                json!({"error": "job control unavailable"}),
            ),
        )
}

fn job_id_parameter() -> utoipa::openapi::path::Parameter {
    ParameterBuilder::new()
        .name("id")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The durable job-run identifier the job history reports"))
        .example(Some(json!("jr_000000000000ffff")))
        .build()
}

fn acl() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("An index's access control"))
        .description(Some(
            "The tokens, grants, expiry, and anonymous-read policy one index is configured with. peryx \
             has no server-wide administrator, so the gate is the index's own: authenticate with HTTP \
             Basic as an access token that holds write over every resource here. Token secrets are never \
             returned, only a marker that one is set.",
        ))
        .security(SecurityRequirement::new("writeToken", Vec::<String>::new()))
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
                        {"name": "writer", "secret": {"configured": true, "redacted": "<redacted>"},
                         "expires_at": null, "grants": [{"resources": ["*"], "actions": ["write", "delete"]}]},
                        {"name": "ci", "secret": {"configured": true, "redacted": "<redacted>"},
                         "expires_at": 1_800_000_000, "grants": [{"resources": ["team/*"], "actions": ["read"]}]}
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
             index. It is built from configuration and request context, without reading artifact metadata.",
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
            "Version, health, counters, and the configured indexes, each filtered to the caller's \
             class. Version, role, coarse health, and the basic index list are public; the serial, \
             request count, blob backend, per-ecosystem rollups, and metric families need operator \
             authority; each index's upstream hosts, write-token state, and recent writes need \
             administrator authority. The response is `private, no-cache`. The example shows the \
             administrator view.",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "200",
            ResponseBuilder::new().description("Caller-filtered process status").content(
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
                            {"ecosystem": "example", "pages": 128, "reads": 6, "bytes": 64_733_247,
                             "rejected": 0, "writes": 4, "families": {"metadata": 37}}
                        ],
                        "metric_families": [
                            {"key": "metadata", "label": "metadata hits",
                             "roles": ["cached", "hosted", "virtual"]}
                        ],
                        "indexes": [
                            {"name": "example", "route": "example", "kind": "cached", "layers": [],
                             "writes": false, "volatile_deletes": false, "write_to": null,
                             "upstream": {"url": "https://upstream.example/artifacts/", "auth": {"kind": "none", "redacted": null}, "status": "configured", "offline": false},
                             "hosted": null, "resource_count": 128, "write_count": 0, "recent_writes": []},
                            {"name": "hosted", "route": "hosted", "kind": "hosted", "layers": [],
                             "writes": true, "volatile_deletes": true, "write_to": null, "upstream": null,
                             "hosted": {"volatile": true, "write_token": {"configured": true, "redacted": "<redacted>"}},
                             "resource_count": 2, "write_count": 4,
                             "recent_writes": [{"resource": "widget", "artifact": "widget-1.0.bin",
                                                "group": "1.0", "written_at": "2026-01-01T00:00:00Z", "size": 1832}]},
                            {"name": "root/artifacts", "route": "root/artifacts", "kind": "virtual",
                             "layers": ["hosted", "example"], "writes": true, "volatile_deletes": true,
                             "write_to": "hosted",
                             "upstream": null, "hosted": null, "resource_count": 0, "write_count": 0,
                             "recent_writes": []}
                        ]
                    })))
                    .build(),
            ),
        )
}

fn health() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Process liveness"))
        .description(Some(
            "Returns a fixed public document while the HTTP process can answer requests. Local-store and upstream failures do not fail liveness.",
        ))
        .response(
            "200",
            api_json_response("The process is live", json!({"status": "live"})),
        )
}

fn readiness() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Read or write readiness"))
        .description(Some(
            "Checks the bounded local metadata and blob-store dependencies used to serve artifact requests. Set `writes=true` to require a writer. The probe does not enumerate repositories or contact upstreams.",
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
            api_json_response("The requested traffic class is ready", json!({"status": "ready"})),
        )
        .response(
            "503",
            api_json_response(
                "A required local dependency is unavailable or write traffic reached a replica",
                json!({"status": "not_ready"}),
            ),
        )
}

fn stats() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Usage statistics"))
        .description(Some(
            "Counters aggregated off the request path, drillable: no parameters for per-index totals, \
             `?index={route}` for one index's resources, `&resource={name}` for one resource's artifacts. \
             The tree names repositories and resources, so it needs operator authority and is never \
             cached; a repository token reads its own usage through `/+analytics/*` instead. Counters \
             are grouped by the role that owns them: a neutral `base` group every index reports, a \
             `cached` group only a caching index fills, a `hosted` group only a writable store fills, \
             and an `ecosystem` map of the driver's own counters (an adapter-specific metadata family under \
             `metadata`).",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "401",
            ResponseBuilder::new().description("No valid operator credential was presented"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The credential holds no operator grant"),
        )
        .response(
            "503",
            api_json_response(
                "Authentication or authorization is unavailable",
                json!({"error": "stats service unavailable"}),
            ),
        )
        .parameter(
            ParameterBuilder::new()
                .name("index")
                .parameter_in(ParameterIn::Query)
                .description(Some("Drill into one index's resources"))
                .example(Some(json!("root/artifacts"))),
        )
        .parameter(
            ParameterBuilder::new()
                .name("resource")
                .parameter_in(ParameterIn::Query)
                .description(Some("With `index`, drill into one resource's artifacts"))
                .example(Some(json!("widget"))),
        )
        .response(
            "200",
            ResponseBuilder::new()
                .description("The counters at the requested depth")
                .content(
                    "application/json",
                    ContentBuilder::new()
                        .example(Some(json!({
                            "root/artifacts": {
                                "base": {"pages": 12, "reads": 6, "bytes": 64_733_247, "rejected": 0},
                                "cached": {"refreshes": 2, "changed": 1, "stale_served": 0, "upstream_errors": 0},
                                "hosted": {"writes": 0},
                                "ecosystem": {"metadata": 6}
                            }
                        })))
                        .build(),
                ),
        )
}

fn analytics_interval() -> serde_json::Value {
    json!({
        "from_day": 19_722,
        "to_day": 19_752,
        "from_unix": 1_703_980_800_i64,
        "to_unix": 1_706_659_200_i64,
        "retained_from_day": 19_387,
        "window_clamped_to_retention": false,
    })
}

/// The query parameters, security, and failure responses shared by every `/+analytics/*` view. An
/// operator query omits `repository`; a repository query names an index route the caller can read.
fn analytics_query(operation: OperationBuilder) -> OperationBuilder {
    let mut operation = operation
        .tag("operations")
        .security(SecurityRequirement::new("writeToken", Vec::<String>::new()))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "400",
            api_json_response(
                "The limit, cursor, time range, or repository filter is invalid",
                json!({"error": "limit must be between 1 and 100"}),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid local user credential or repository token was presented"),
        )
        .response(
            "403",
            ResponseBuilder::new().description("The credential cannot inspect this view or repository"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The repository does not exist or is not visible to the caller"),
        )
        .response(
            "503",
            api_json_response(
                "Authentication, authorization, or analytics storage is unavailable",
                json!({"error": "analytics service unavailable"}),
            ),
        );
    for (name, description, example) in [
        (
            "repository",
            "Index route to scope the query to; omit for an operator-wide query, at most 512 bytes",
            json!("root/artifacts"),
        ),
        (
            "from",
            "Minimum Unix timestamp, floored to its UTC day",
            json!(1_703_980_800_i64),
        ),
        (
            "to",
            "Maximum Unix timestamp, floored to its UTC day",
            json!(1_706_659_200_i64),
        ),
        (
            "cursor",
            "Opaque cursor from the prior page's next_cursor",
            json!("MjU"),
        ),
        ("limit", "Rows to return, from 1 through 100; defaults to 25", json!(25)),
    ] {
        operation = operation.parameter(
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Query)
                .description(Some(description))
                .example(Some(example)),
        );
    }
    operation
}

fn analytics_top() -> OperationBuilder {
    analytics_query(
        OperationBuilder::new()
            .summary(Some("Most-read resources"))
            .description(Some(
                "Reads and bytes grouped by repository and resource over the resolved window, ordered by reads, \
                 bytes, repository, then resource.",
            )),
    )
    .response(
        "200",
        api_json_response(
            "The most-read resources, newest window first",
            json!({
                "resources": [{"repository": "root/content", "resource": "artifact-a", "reads": 42, "bytes": 64_733_247}],
                "interval": analytics_interval(),
                "next_cursor": null,
            }),
        ),
    )
}

fn analytics_unused() -> OperationBuilder {
    analytics_query(
        OperationBuilder::new()
            .summary(Some("Unused resources"))
            .description(Some(
                "Resources with durable lifetime reads but none inside the window, ordered by lifetime reads and \
                 repository. A `window_clamped_to_retention` interval marks results \
                 assessed only over retained data.",
            )),
    )
    .response(
        "200",
        api_json_response(
            "Resources idle across the window",
            json!({
                "unused": [{"repository": "root/content", "resource": "legacy-artifact", "lifetime_reads": 7}],
                "interval": analytics_interval(),
                "next_cursor": null,
            }),
        ),
    )
}

fn analytics_groups() -> OperationBuilder {
    analytics_query(
        OperationBuilder::new()
            .summary(Some("Per-group usage"))
            .description(Some(
                "Reads and bytes grouped by repository, resource, and owner-defined group over the window. A null \
                 group means the ecosystem supplied no grouping dimension.",
            )),
    )
    .response(
        "200",
        api_json_response(
            "The highest-usage groups",
            json!({
                "groups": [
                    {"repository": "root/content", "resource": "artifact-a", "group": "stable", "reads": 30, "bytes": 48_000_000}
                ],
                "interval": analytics_interval(),
                "next_cursor": null,
            }),
        ),
    )
}

fn analytics_sources() -> OperationBuilder {
    analytics_query(
        OperationBuilder::new()
            .summary(Some("Per-source usage"))
            .description(Some(
                "Reads and bytes grouped by the routed upstream a cache miss fetched from; a null source is \
                 the local store. The source dimension is operator-scoped, so a repository-only credential \
                 cannot inspect this view.",
            )),
    )
    .response(
        "200",
        api_json_response(
            "The highest-usage sources",
            json!({
                "sources": [
                    {"repository": "root/content", "resource": "artifact-b", "source": "example", "reads": 40, "bytes": 60_000_000}
                ],
                "interval": analytics_interval(),
                "next_cursor": null,
            }),
        ),
    )
}

fn analytics_timeline() -> OperationBuilder {
    analytics_query(
        OperationBuilder::new()
            .summary(Some("Usage over time"))
            .description(Some(
                "Reads and bytes bucketed by UTC day, ascending, each carrying explicit half-open \
                 `[start_unix, end_unix)` bounds for the day it aggregates.",
            )),
    )
    .response(
        "200",
        api_json_response(
            "The daily usage series",
            json!({
                "buckets": [
                    {"day": 19_752, "start_unix": 1_706_572_800_i64, "end_unix": 1_706_659_200_i64, "reads": 12, "bytes": 9_000_000}
                ],
                "interval": analytics_interval(),
                "next_cursor": null,
            }),
        ),
    )
}

fn policy_decisions_example() -> serde_json::Value {
    json!({
        "decisions": [{
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "repository": "private",
            "resource": "example",
            "group": "1.0",
            "artifact": "example-1.0.bin",
            "source": "example",
            "action": "serve",
            "state": "deny",
            "rule": "blocked-resource",
            "reason": "resource is blocked",
            "evaluated_at_unix": 1_800_000_000,
            "input_generation": {"repository": 42, "catalog": 7, "policy": 3},
            "next_eligible_at_unix": null,
            "fresh": true
        }],
        "next_cursor": "pd_000000000000002a"
    })
}

fn pql_query() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Run a PQL query"))
        .description(Some(
            "Runs one read-only Peryx Query Language (PQL) query over a typed domain and returns a bounded page of \
             rows. The `query` is a small textual DSL - `from <domain> [join <domain> on <keys>] [where ...] \
             [select ...] [aggregate ... by ...] [order by ...] [limit n]` - and `params` binds `:name` placeholders \
             out of band, so a value never changes the query's structure. Two domains are served: `policy.decisions` \
             and `usage.reads`, and a bounded declared join correlates them on their shared `repository` and \
             `resource` keys. The caller's authorized scope is injected by the evaluator and cannot be named or \
             widened; columns above the caller's classification are dropped, and operator-classified results are \
             never cached. `next_cursor`, presented back, resumes the next page and is refused if the caller's scope \
             has changed. Authenticate with an ecosystem credential to read one repository, or a local administrator to \
             read operator-wide.",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .request_body(Some(
            RequestBodyBuilder::new()
                .required(Some(Required::True))
                .content(
                    "application/json",
                    ContentBuilder::new()
                        .example(Some(json!({
                            "query": "from policy.decisions where repository == :repo and state == \"deny\" \
                                      order by evaluated_at desc limit 25",
                            "params": {"repo": "artifact-cache"},
                            "cursor": null
                        })))
                        .build(),
                )
                .build(),
        ))
        .response(
            "200",
            api_json_response(
                "One bounded page of typed rows",
                json!({
                    "rows": [{
                        "repository": "artifact-cache",
                        "resource": "artifact-a",
                        "state": "deny",
                        "action": "serve",
                        "evaluated_at": 1_800_000_000,
                        "fresh": true
                    }],
                    "next_cursor": null
                }),
            ),
        )
        .response(
            "400",
            api_json_response(
                "The query did not parse, is invalid, is over budget, or the cursor no longer matches the scope",
                json!({"error": "the query is not valid"}),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid credential was presented"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The caller cannot read the domain; its existence is not disclosed"),
        )
        .response("415", ResponseBuilder::new().description("The request is not JSON"))
        .response(
            "422",
            ResponseBuilder::new().description("The JSON request body is invalid"),
        )
        .response(
            "503",
            api_json_response(
                "The query backend is unavailable",
                json!({"error": "the query backend is unavailable"}),
            ),
        )
}

fn policy_decisions() -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("operations")
        .summary(Some("Repository policy decisions"))
        .description(Some(
            "Returns bounded policy decision history. Administrators may inspect all repositories or select one. \
             Repository readers, publishers, and authorized ecosystem credentials may inspect a selected \
             repository. The server operator role has no repository access. Records contain artifact subjects and \
             matched rule IDs without credentials or request headers. `fresh` becomes false after repository data, \
             catalog, or policy inputs change.",
        ))
        .security(SecurityRequirement::new("writeToken", Vec::<String>::new()))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "200",
            api_json_response("The matching decisions, newest first", policy_decisions_example()),
        )
        .response(
            "400",
            api_json_response(
                "The limit, cursor, or text filter is invalid",
                json!({"error": "limit must be between 1 and 100"}),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid local or ecosystem credential was presented"),
        )
        .response(
            "403",
            ResponseBuilder::new().description("The credential cannot inspect policy decisions"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The repository does not exist or is not available to the local user"),
        )
        .response(
            "500",
            api_json_response(
                "The decision store could not complete the query",
                json!({"error": "policy decision query failed"}),
            ),
        )
        .response(
            "503",
            api_json_response(
                "Authentication or authorization storage is unavailable",
                json!({"error": "policy decision service unavailable"}),
            ),
        );
    for (name, description, example) in [
        (
            "repository",
            "Repository route to inspect, at most 512 bytes",
            json!("private"),
        ),
        (
            "resource",
            "Filter to one resource's decisions, at most 512 bytes",
            json!("example"),
        ),
        ("state", "Filter by `allow`, `deny`, or `wait`", json!("deny")),
        (
            "rule",
            "Filter by matched rule ID, at most 512 bytes",
            json!("blocked-resource"),
        ),
        ("source", "Filter by routed source, at most 512 bytes", json!("example")),
        ("from", "Minimum evaluation Unix timestamp", json!(1_700_000_000)),
        ("to", "Maximum evaluation Unix timestamp", json!(1_800_000_000)),
        (
            "cursor",
            "Exclusive cursor from the prior page",
            json!("pd_000000000000002a"),
        ),
        ("limit", "Rows to return, from 1 through 100; defaults to 25", json!(25)),
    ] {
        let parameter = ParameterBuilder::new()
            .name(name)
            .parameter_in(ParameterIn::Query)
            .description(Some(description))
            .example(Some(example));
        operation = operation.parameter(parameter);
    }
    operation
}

fn revocation_example() -> serde_json::Value {
    json!({
        "digest": {"sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},
        "reason": "compromised build host",
        "created_by": "usr_550e8400e29b41d4a716446655440000",
        "created_at_unix": 1_800_000_000,
        "state": {"status": "active"},
        "revision": 1
    })
}

fn digest_parameter() -> utoipa::openapi::path::Parameter {
    ParameterBuilder::new()
        .name("digest")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("Canonical `sha256:<64 lowercase hex>` artifact digest"))
        .example(Some(json!(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )))
        .build()
}

fn administrator_errors(operation: OperationBuilder) -> OperationBuilder {
    operation
        .response(
            "401",
            ResponseBuilder::new().description("No valid local user credential was presented"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The caller cannot discover this record, or it does not exist"),
        )
        .response(
            "503",
            api_json_response(
                "Authentication, authorization, or revocation storage is unavailable",
                json!({"error": "revocation service unavailable"}),
            ),
        )
}

fn inspect_revocation() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Inspect a digest revocation"))
            .description(Some(
                "Returns the current record without changing lifecycle, retention, or policy state.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .parameter(digest_parameter())
            .response(
                "200",
                api_json_response("The current revocation record", revocation_example()),
            )
            .response(
                "400",
                api_json_response(
                    "The digest is not canonical SHA-256",
                    json!({"error": "invalid digest"}),
                ),
            ),
    )
}

fn list_revocations() -> OperationBuilder {
    let mut operation = administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("List digest revocations"))
            .description(Some(
                "Returns a bounded page of current records in canonical digest order. Lifted records remain visible to administrators.",
            ))
            .security(SecurityRequirement::new(
                "administratorPassword",
                Vec::<String>::new(),
            ))
            .response(
                "200",
                api_json_response(
                    "The matching current records",
                    json!({"revocations": [revocation_example()], "next_cursor": null}),
                ),
            )
            .response(
                "400",
                api_json_response(
                    "The cursor or limit is invalid",
                    json!({"error": "invalid revocation cursor"}),
                ),
            ),
    );
    for (name, description, example) in [
        ("status", "Filter by `active` or `lifted`", json!("active")),
        (
            "cursor",
            "Exclusive canonical digest from the prior page",
            json!("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        ),
        ("limit", "Rows to return, from 1 through 100; defaults to 25", json!(25)),
    ] {
        operation = operation.parameter(
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Query)
                .description(Some(description))
                .example(Some(example)),
        );
    }
    operation
}

fn put_revocation() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Put an active digest revocation"))
            .description(Some(
                "Creates or reopens the digest-addressed record. Retrying the same active reason is idempotent; replacing another active reason conflicts.",
            ))
            .security(SecurityRequirement::new(
                "administratorPassword",
                Vec::<String>::new(),
            ))
            .parameter(digest_parameter())
            .request_body(Some(
                RequestBodyBuilder::new()
                    .required(Some(Required::True))
                    .content(
                        "application/json",
                        ContentBuilder::new()
                            .example(Some(json!({"reason": "compromised build host"})))
                            .build(),
                    )
                    .build(),
            ))
            .response("200", api_json_response("The unchanged active record", revocation_example()))
            .response("201", api_json_response("The created or reopened record", revocation_example()))
            .response(
                "400",
                api_json_response("The digest or reason is invalid", json!({"error": "invalid digest"})),
            )
            .response(
                "409",
                api_json_response(
                    "The active record has another reason",
                    json!({"error": "digest is already revoked"}),
                ),
            )
            .response("413", ResponseBuilder::new().description("The request exceeds the fixed body limit"))
            .response("415", ResponseBuilder::new().description("The request is not JSON"))
            .response("422", ResponseBuilder::new().description("The JSON request body is invalid")),
    )
}

fn lift_revocation() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Lift a digest revocation"))
            .description(Some(
                "Transitions an active record to lifted and retains its original reason, actor, and creation time. Retrying a lift is idempotent.",
            ))
            .security(SecurityRequirement::new(
                "administratorPassword",
                Vec::<String>::new(),
            ))
            .parameter(digest_parameter())
            .response(
                "200",
                api_json_response(
                    "The lifted record",
                    json!({
                        "digest": {"sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},
                        "reason": "compromised build host",
                        "created_by": "usr_550e8400e29b41d4a716446655440000",
                        "created_at_unix": 1_800_000_000,
                        "state": {
                            "status": "lifted",
                            "lifted_by": "usr_98b2271831d647c09a1e6f630cc48ef7",
                            "lifted_at_unix": 1_800_000_100
                        },
                        "revision": 2
                    }),
                ),
            )
            .response(
                "400",
                api_json_response("The digest is not canonical SHA-256", json!({"error": "invalid digest"})),
            ),
    )
}

fn quota_meter_example(committed: u64, reserved: u64, limit: Option<u64>, remaining: Option<u64>) -> serde_json::Value {
    json!({
        "committed": committed,
        "reserved": reserved,
        "limit": limit,
        "remaining": remaining,
    })
}

fn quota_repository_example() -> serde_json::Value {
    json!({
        "repository": "root/artifacts",
        "ecosystem": "example",
        "limits": {
            "max_artifact_bytes": 104_857_600,
            "max_resource_bytes": null,
            "max_accounted_bytes": 10_737_418_240_u64,
            "max_resources": 500,
            "max_groups_per_resource": 100,
            "audit": false
        },
        "artifact_bytes": quota_meter_example(4_294_967_296, 1_048_576, None, None),
        "accounted_bytes": quota_meter_example(3_221_225_472, 1_048_576, Some(10_737_418_240), Some(7_515_144_192)),
        "resources": quota_meter_example(128, 1, Some(500), Some(371))
    })
}

fn quota_summary() -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("operations")
        .summary(Some("Repository quota summary"))
        .description(Some(
            "One bounded page of every repository's quota in configuration order, for a local administrator. Each \
             row pairs the committed and reserved counters the store maintains with the limits the index \
             configures, and reports the remaining headroom, or null when a counter is unlimited. The page omits \
             per-resource and per-artifact detail; name a repository through `/+quota/repository` for one \
             repository. The cursor pages over the static index list, so it stays stable while a reservation \
             changes a counter under it.",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "200",
            api_json_response(
                "One page of repository quotas",
                json!({"repositories": [quota_repository_example()], "next_cursor": null}),
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
            ResponseBuilder::new().description("No valid local user credential was presented"),
        )
        .response(
            "403",
            ResponseBuilder::new().description("An ecosystem credential cannot enumerate repositories"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The caller lacks operator authority to enumerate repositories"),
        )
        .response(
            "503",
            api_json_response(
                "Authentication, authorization, or quota storage is unavailable",
                json!({"error": "quota service unavailable"}),
            ),
        );
    for (name, description, example) in [
        ("cursor", "Opaque cursor from the prior page's next_cursor", json!("Mg")),
        ("limit", "Rows to return, from 1 through 100; defaults to 25", json!(25)),
    ] {
        operation = operation.parameter(
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Query)
                .description(Some(description))
                .example(Some(example)),
        );
    }
    operation
}

fn quota_repository() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Repository quota detail"))
        .description(Some(
            "Returns one repository's quota to local users and ecosystem credentials authorized to read it. The \
             response pairs committed and reserved counters with configured limits and reports remaining headroom, \
             or null for an unlimited counter. It identifies no individual artifact.",
        ))
        .security(SecurityRequirement::new("writeToken", Vec::<String>::new()))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .parameter(
            ParameterBuilder::new()
                .name("repository")
                .parameter_in(ParameterIn::Query)
                .required(Required::True)
                .description(Some("Index route to inspect, at most 512 bytes"))
                .example(Some(json!("root/artifacts"))),
        )
        .response(
            "200",
            api_json_response("The repository's quota", quota_repository_example()),
        )
        .response(
            "400",
            api_json_response(
                "The repository selector is missing or invalid",
                json!({"error": "repository is required"}),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid local or ecosystem credential was presented"),
        )
        .response(
            "403",
            ResponseBuilder::new().description("The credential cannot read this repository"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The repository does not exist or is not visible to the caller"),
        )
        .response(
            "503",
            api_json_response(
                "Authentication, authorization, or quota storage is unavailable",
                json!({"error": "quota service unavailable"}),
            ),
        )
}

fn grant_example() -> serde_json::Value {
    json!({
        "id": "rg_7573725f31322f7265706f7369746f72795f726561646572",
        "user": "usr_550e8400e29b41d4a716446655440000",
        "role": "repository_reader",
        "scope": {"kind": "repository", "name": "root/artifacts"},
        "version": 1,
        "granted_by": "usr_98b2271831d647c09a1e6f630cc48ef7",
        "granted_at_unix": 1_800_000_000
    })
}

fn grant_id_parameter() -> utoipa::openapi::path::Parameter {
    ParameterBuilder::new()
        .name("id")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("Opaque, stable grant identifier from a create or list response"))
        .example(Some(json!("rg_7573725f31322f7265706f7369746f72795f726561646572")))
        .build()
}

fn list_grants() -> OperationBuilder {
    let mut operation = administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("List role grants"))
            .description(Some(
                "Returns a bounded page of role grants in stable identifier order. A `user` filter reads one \
                 user's grants and a `resource` filter one reach's; both need administration authority over what \
                 they select, so a repository administrator may list its own repository but not the whole server.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .response(
                "200",
                api_json_response(
                    "The matching grants and the cursor that resumes the next page",
                    json!({"grants": [grant_example()], "next_cursor": null}),
                ),
            )
            .response(
                "403",
                ResponseBuilder::new().description("The caller holds no administration authority over the selection"),
            ),
    );
    for (name, description, example) in [
        (
            "user",
            "Filter to one user's grants",
            json!("usr_550e8400e29b41d4a716446655440000"),
        ),
        (
            "resource",
            "Filter to one reach: `server` or `repository/<name>`",
            json!("repository/root/artifacts"),
        ),
        (
            "cursor",
            "Opaque identifier from the prior page",
            json!("rg_7573725f31322f7265706f7369746f72795f726561646572"),
        ),
        ("limit", "Rows to return, from 1 through 100; defaults to 25", json!(25)),
    ] {
        operation = operation.parameter(
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Query)
                .description(Some(description))
                .example(Some(example)),
        );
    }
    operation.response(
        "400",
        api_json_response(
            "A filter or limit is invalid",
            json!({"error": "invalid resource filter"}),
        ),
    )
}

fn create_grant() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Grant a role to a user"))
            .description(Some(
                "Binds a user to a fixed role over a reach. Idempotent: re-asserting an existing binding refreshes \
                 its audit fields and advances its version rather than conflicting. The caller may bind only a reach \
                 it administers and never one it lacks, so a grant cannot escalate the caller's own authority. The \
                 response carries the binding, an `ETag` a later revoke matches against, and a `Location`.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .request_body(Some(
                RequestBodyBuilder::new()
                    .required(Some(Required::True))
                    .content(
                        "application/json",
                        ContentBuilder::new()
                            .example(Some(json!({
                                "user": "usr_550e8400e29b41d4a716446655440000",
                                "role": "repository_reader",
                                "scope": {"kind": "repository", "name": "root/artifacts"}
                            })))
                            .build(),
                    )
                    .build(),
            ))
            .response("200", api_json_response("The re-asserted binding", grant_example()))
            .response("201", api_json_response("The newly created binding", grant_example()))
            .response(
                "403",
                ResponseBuilder::new().description("The caller cannot administer the target reach"),
            )
            .response("415", ResponseBuilder::new().description("The request is not JSON"))
            .response(
                "422",
                api_json_response(
                    "The body is invalid, the user is unknown or disabled, or the role does not apply to the scope",
                    json!({"error": "user does not exist"}),
                ),
            ),
    )
}

fn inspect_grant() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Inspect a role grant"))
            .description(Some(
                "Returns one binding and the `ETag` that its revocation precondition matches. A caller that cannot \
                 administer the binding's reach cannot tell it apart from one that does not exist.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .parameter(grant_id_parameter())
            .response("200", api_json_response("The current binding", grant_example())),
    )
}

fn revoke_grant() -> OperationBuilder {
    administrator_errors(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Revoke a role grant"))
            .description(Some(
                "Removes a binding, conditional on an `If-Match` naming the version the caller observed. A revoke \
                 that raced a re-assertion fails the precondition rather than dropping the newer grant. The removal \
                 is reflected by the next authorization decision without a restart.",
            ))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .parameter(grant_id_parameter())
            .response("204", ResponseBuilder::new().description("The binding was removed"))
            .response(
                "400",
                api_json_response(
                    "The `If-Match` precondition is malformed",
                    json!({"error": "invalid If-Match precondition"}),
                ),
            )
            .response(
                "412",
                api_json_response(
                    "The binding is at a different version than the precondition named",
                    json!({"error": "grant version precondition failed"}),
                ),
            )
            .response(
                "428",
                api_json_response(
                    "The request carried no `If-Match` precondition",
                    json!({"error": "revocation requires an If-Match precondition"}),
                ),
            ),
    )
}

fn retention_request_body() -> utoipa::openapi::request_body::RequestBody {
    RequestBodyBuilder::new()
        .required(Some(Required::True))
        .content(
            "application/json",
            ContentBuilder::new()
                .example(Some(json!({
                    "repository": "root/artifacts",
                    "keep": [{"selector": "keep-latest", "count": 3}],
                    "expire": [{"selector": "age", "older_than_seconds": 7_776_000}],
                    "cursor": null,
                    "limit": 100
                })))
                .build(),
        )
        .build()
}

fn retention_candidate_example() -> serde_json::Value {
    json!({
        "resource": "example",
        "group": "1.0",
        "artifact": "example-1.0.bin",
        "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "class": "hosted",
        "visibility": "active",
        "bytes": 20_480,
        "outcome": "remove",
        "rule": "age",
        "retained_groups": ["2.0"]
    })
}

fn retention_plan() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Preview a repository retention plan"))
        .description(Some(
            "Evaluates the supplied keep/expire rules against one repository and returns a bounded, ordered page \
             of removal candidates without changing any metadata or blob. `summary` carries the policy version and \
             metadata frontier the page read; `next_cursor` resumes the next page and, presented back, rejects a \
             plan whose repository has since changed. Requires a local administrator.",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .request_body(Some(retention_request_body()))
        .response(
            "200",
            api_json_response(
                "One ordered page of candidates",
                json!({
                    "summary": {"policy_version": 42, "frontier": {"repository": 7, "catalog": 3, "policy": 2}},
                    "candidates": [retention_candidate_example()],
                    "next_cursor": null
                }),
            ),
        )
        .response(
            "400",
            api_json_response(
                "The cursor or limit is invalid",
                json!({"error": "limit must be between 1 and 1000"}),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid local administrator credential was presented"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The caller cannot inspect the repository, or it plans no retention"),
        )
        .response(
            "409",
            api_json_response(
                "The cursor is stale: the repository changed since it was issued",
                json!({"error": "the plan cursor is stale: the repository changed"}),
            ),
        )
        .response(
            "413",
            ResponseBuilder::new().description("The request exceeds the fixed body limit"),
        )
        .response("415", ResponseBuilder::new().description("The request is not JSON"))
        .response(
            "422",
            ResponseBuilder::new().description("The JSON request body is invalid"),
        )
        .response(
            "429",
            api_json_response(
                "Too many concurrent plans for this repository",
                json!({"error": "too many concurrent retention plans for this repository"}),
            ),
        )
        .response(
            "500",
            api_json_response("The plan could not be read", json!({"error": "retention plan failed"})),
        )
        .response(
            "503",
            api_json_response(
                "Authentication storage is unavailable",
                json!({"error": "retention service unavailable"}),
            ),
        )
}

fn retention_export() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Export a repository retention plan"))
        .description(Some(
            "Streams the whole plan as JSON Lines: a first line carrying the `summary` identity, then one candidate \
             per line. The `ETag` is the plan identity, and the export is resumable from its documented boundary by \
             presenting a prior page's `cursor`, which is refused when the repository has changed. The stream is \
             unique to one snapshot, so byte ranges do not apply. Requires a local administrator.",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .request_body(Some(retention_request_body()))
        .response(
            "200",
            text_response(
                "The plan as JSON Lines, the identity first",
                "application/x-ndjson",
                "{\"summary\":{\"policy_version\":42,\"frontier\":{\"repository\":7,\"catalog\":3,\"policy\":2}}}\n\
                 {\"resource\":\"example\",\"group\":\"1.0\",\"artifact\":\"example-1.0.bin\",\
                 \"digest\":\"sha256:0123\",\"class\":\"hosted\",\"visibility\":\"active\",\"bytes\":20480,\
                 \"outcome\":\"remove\",\"rule\":\"age\"}\n",
            ),
        )
        .response(
            "400",
            api_json_response(
                "The cursor is invalid",
                json!({"error": "invalid retention plan cursor"}),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid local administrator credential was presented"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The caller cannot inspect the repository, or it plans no retention"),
        )
        .response(
            "409",
            api_json_response(
                "The cursor is stale: the repository changed since it was issued",
                json!({"error": "the plan cursor is stale: the repository changed"}),
            ),
        )
        .response(
            "413",
            ResponseBuilder::new().description("The request exceeds the fixed body limit"),
        )
        .response("415", ResponseBuilder::new().description("The request is not JSON"))
        .response(
            "422",
            ResponseBuilder::new().description("The JSON request body is invalid"),
        )
        .response(
            "429",
            api_json_response(
                "Too many concurrent plans for this repository",
                json!({"error": "too many concurrent retention plans for this repository"}),
            ),
        )
        .response(
            "500",
            api_json_response("The plan could not be read", json!({"error": "retention plan failed"})),
        )
        .response(
            "503",
            api_json_response(
                "Authentication storage is unavailable",
                json!({"error": "retention service unavailable"}),
            ),
        )
}

fn scoped_token_example() -> serde_json::Value {
    json!({
        "id": "tok_550e8400e29b41d4a716446655440000",
        "name": "ci-write",
        "reach": {"kind": "repository", "name": "hosted"},
        "actions": ["read", "write"],
        "created_by": "usr_98b2271831d647c09a1e6f630cc48ef7",
        "created_at_unix": 1_800_000_000,
        "expires_at": 1_800_600_000,
        "revoked_at": null,
        "revision": 1
    })
}

fn token_id_parameter() -> utoipa::openapi::path::Parameter {
    ParameterBuilder::new()
        .name("id")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The stable token identifier returned at creation"))
        .example(Some(json!("tok_550e8400e29b41d4a716446655440000")))
        .build()
}

fn token_errors(operation: OperationBuilder) -> OperationBuilder {
    operation
        .tag("operations")
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "401",
            ResponseBuilder::new().description("No valid local user credential was presented"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The caller cannot manage this reach or token, or it does not exist"),
        )
        .response(
            "503",
            api_json_response(
                "Authentication, authorization, or token storage is unavailable",
                json!({"error": "token service unavailable"}),
            ),
        )
}

fn create_token() -> OperationBuilder {
    token_errors(
        OperationBuilder::new()
            .summary(Some("Create a scoped token"))
            .description(Some(
                "Mints a named token over a reach the caller is authorized to grant: omit `repository` for a \
                 server-wide token, which requires administrator authority, or name a repository route for a \
                 token scoped to it, which requires repository write there. The response reveals the secret \
                 once; later reads never do. A repository manager cannot mint a server-wide or \
                 cross-repository token.",
            ))
            .request_body(Some(
                RequestBodyBuilder::new()
                    .required(Some(Required::True))
                    .content(
                        "application/json",
                        ContentBuilder::new()
                            .example(Some(json!({
                                "name": "ci-write",
                                "repository": "hosted",
                                "actions": ["read", "write"],
                                "expires_at": 1_800_600_000
                            })))
                            .build(),
                    )
                    .build(),
            )),
    )
    .response(
        "201",
        api_json_response(
            "The created token and its one-time secret",
            json!({"token": scoped_token_example(), "secret": "peryx_XSm1t3nR9k...redacted"}),
        ),
    )
    .response(
        "400",
        api_json_response(
            "The name, actions, or expiry is invalid",
            json!({"error": "at least one action is required"}),
        ),
    )
    .response(
        "413",
        ResponseBuilder::new().description("The request exceeds the fixed body limit"),
    )
    .response("415", ResponseBuilder::new().description("The request is not JSON"))
    .response(
        "422",
        ResponseBuilder::new().description("The JSON request body is invalid"),
    )
}

fn list_tokens() -> OperationBuilder {
    let mut operation = token_errors(
        OperationBuilder::new()
            .summary(Some("List scoped tokens"))
            .description(Some(
                "Returns a bounded page of token metadata over one reach in stable id order, secrets never \
                 included. Omit `repository` for the server reach, or name a repository route for its tokens.",
            )),
    )
    .response(
        "200",
        api_json_response(
            "The matching token metadata",
            json!({"tokens": [scoped_token_example()], "next_cursor": null}),
        ),
    )
    .response(
        "400",
        api_json_response("The limit is invalid", json!({"error": "invalid limit"})),
    );
    for (name, description, example) in [
        (
            "repository",
            "Index route to list tokens for; omit for server-wide tokens",
            json!("hosted"),
        ),
        (
            "cursor",
            "Exclusive token id from the prior page",
            json!("tok_550e8400e29b41d4a716446655440000"),
        ),
        ("limit", "Rows to return, from 1 through 100; defaults to 25", json!(25)),
    ] {
        operation = operation.parameter(
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Query)
                .description(Some(description))
                .example(Some(example)),
        );
    }
    operation
}

fn inspect_token() -> OperationBuilder {
    token_errors(
        OperationBuilder::new()
            .summary(Some("Inspect a scoped token"))
            .description(Some(
                "Returns one token's metadata, revoked or live, without its secret.",
            ))
            .parameter(token_id_parameter()),
    )
    .response("200", api_json_response("The token metadata", scoped_token_example()))
}

fn rotate_token() -> OperationBuilder {
    token_errors(
        OperationBuilder::new()
            .summary(Some("Rotate a scoped token"))
            .description(Some(
                "Issues a new secret for the token, invalidating the prior one and leaving its id, reach, and \
                 actions unchanged. The response reveals the new secret once. A revoked token cannot be \
                 rotated. A failed rotation leaves the prior secret valid.",
            ))
            .parameter(token_id_parameter()),
    )
    .response(
        "200",
        api_json_response(
            "The rotated token and its new one-time secret",
            json!({"token": scoped_token_example(), "secret": "peryx_a7Qb2Lp0Zx...redacted"}),
        ),
    )
}

fn revoke_token() -> OperationBuilder {
    token_errors(
        OperationBuilder::new()
            .summary(Some("Revoke a scoped token"))
            .description(Some(
                "Revokes the token so it stops authenticating on its next request, retaining the record and \
                 its lifecycle evidence. Idempotent: revoking an already-revoked token returns its unchanged \
                 record.",
            ))
            .parameter(token_id_parameter()),
    )
    .response(
        "200",
        api_json_response("The revoked token metadata", scoped_token_example()),
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
                 peryx_requests_total 128\n",
            ),
        )
}

fn openapi_endpoint() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("OpenAPI schema"))
        .response(
            "200",
            ResponseBuilder::new()
                .description("OpenAPI 3.1 schema")
                .content("application/json", ContentBuilder::new().build()),
        )
}
