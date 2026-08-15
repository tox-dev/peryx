use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::{PathsBuilder, ResponseBuilder, SecurityRequirement};

#[must_use]
pub fn availability_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+analytics/completeness",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, analytics_completeness())
                .build(),
        )
        .path(
            "/+availability/topology",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, availability_topology())
                .build(),
        )
        .path(
            "/+availability/topology/stream",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, availability_topology_stream())
                .build(),
        )
        .path(
            "/+availability/operations",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, availability_operations())
                .build(),
        )
        .path(
            "/+availability/placements",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, availability_placements())
                .build(),
        )
        .path(
            "/+availability/placements/{digest}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, availability_blob_placements())
                .build(),
        )
}

fn analytics_completeness() -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("operations")
        .summary(Some("Distributed analytics completeness"))
        .description(Some(
            "Reports whether accepted analytics totals cover each configured producer. Repository credentials see the verdict and scoped totals; operators also see producer and cluster frontiers.",
        ))
        .security(SecurityRequirement::new("writeToken", Vec::<String>::new()))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response("400", json_response("Invalid query", json!({"error": "limit must be between 1 and 100"})))
        .response("401", ResponseBuilder::new().description("No valid credential was presented"))
        .response("403", ResponseBuilder::new().description("The credential cannot inspect this result"))
        .response("404", ResponseBuilder::new().description("The repository is unavailable to the caller"))
        .response("503", json_response("Analytics are unavailable", json!({"error": "analytics service unavailable"})))
        .response(
            "200",
            json_response(
                "The completeness verdict and accepted frontiers",
                json!({
                    "completeness": "delayed",
                    "totals": {"reads": 128, "bytes": 64_733_247},
                    "frontier_day": 19_752,
                    "required_day": 19_752,
                    "lag_days": 1,
                    "producers": [
                        {"producer": "east-writer", "dc": "east", "state": "complete", "accepted_epoch": 1, "accepted_day": 19_752},
                        {"producer": "west-writer", "dc": "west", "state": "delayed", "accepted_epoch": 1, "accepted_day": 19_750}
                    ]
                }),
            ),
        );
    for (name, description, example) in [
        (
            "repository",
            "Repository route; omit for an operator query",
            json!("root/artifacts"),
        ),
        ("from", "Minimum Unix timestamp", json!(1_703_980_800_i64)),
        ("to", "Maximum Unix timestamp", json!(1_706_659_200_i64)),
        ("cursor", "Cursor from the prior page", json!("MjU")),
        ("limit", "Rows to return, from 1 through 100", json!(25)),
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

fn availability_topology() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Availability topology snapshot"))
        .description(Some(
            "Returns one role-filtered topology snapshot. Operators see liveness and committed frontiers; administrators also see peer addresses.",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "200",
            json_response(
                "The availability topology snapshot",
                json!({
                    "mode": "dc",
                    "group": "east",
                    "captured_at": 1_800_000_000,
                    "node_count": 2,
                    "local": {"role": "writer", "liveness": "live", "frontier": 42},
                    "nodes": [
                        {"node": "writer-a", "dc": "east-1", "role": "writer", "local": true, "liveness": "live", "frontier": 42, "address": "10.0.0.1:8080"},
                        {"node": "replica-b", "dc": "east-2", "role": "replica", "local": false, "liveness": "unknown", "address": "10.0.0.2:8080"}
                    ]
                }),
            ),
        )
}

fn availability_topology_stream() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Availability topology stream"))
        .description(Some(
            "Streams the role-filtered topology snapshot when its state changes. Slow readers receive the latest snapshot without a backlog.",
        ))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "200",
            ResponseBuilder::new()
                .description("Topology snapshot events")
                .content(
                    "text/event-stream",
                    ContentBuilder::new()
                        .example(Some(json!("id: 1\nevent: topology\ndata: {\"mode\":\"dc\",\"group\":\"east\",\"nodes\":[]}\n\n")))
                        .build(),
                ),
        )
}

fn availability_blob_placements() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Blob placement across datacenters"))
        .description(Some("Returns datacenter placement state without backend paths."))
        .parameter(
            ParameterBuilder::new()
                .name("digest")
                .parameter_in(ParameterIn::Path)
                .description(Some("Content digest"))
                .example(Some(json!("sha256:0f1e"))),
        )
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "200",
            json_response(
                "The blob placements",
                json!({
                    "digest": "sha256:0f1e",
                    "datacenters": [
                        {"data_center": "east-1", "status": "verified", "size": 4096, "updated_at": 1_800_000_000},
                        {"data_center": "west-2", "status": "pending", "updated_at": 1_800_000_050}
                    ]
                }),
            ),
        )
}

fn availability_operations() -> OperationBuilder {
    paged_health_operation(
        "Pending operations health",
        "Returns aggregate operation health to operators and bounded operation rows to administrators.",
        "operation id",
        json!("op-0f1e"),
        json!({
            "captured_at": 1_800_000_000,
            "health": {"pending": 2, "published": 5, "failed": 1, "expired": 1, "total": 9},
            "rows": [{"operation": "op-0f1e", "status": "pending", "updated_at": 1_800_000_000, "expires_at": 1_800_000_600}],
            "next_cursor": "op-0f1e"
        }),
    )
}

fn availability_placements() -> OperationBuilder {
    paged_health_operation(
        "Artifact placement health",
        "Returns aggregate byte availability to operators and bounded digest rows to administrators.",
        "digest",
        json!("sha256:0f1e"),
        json!({
            "captured_at": 1_800_000_000,
            "health": {"local": 3, "remote_only": 1, "unavailable": 2, "total": 6},
            "rows": [{"digest": "sha256:0f1e", "source": "proxy", "availability": "remote_only"}],
            "next_cursor": "sha256:0f1e"
        }),
    )
}

fn paged_health_operation(
    summary: &'static str,
    description: &'static str,
    cursor_kind: &'static str,
    cursor: serde_json::Value,
    example: serde_json::Value,
) -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some(summary))
        .description(Some(description))
        .parameter(
            ParameterBuilder::new()
                .name("cursor")
                .parameter_in(ParameterIn::Query)
                .description(Some(format!("Resume after this {cursor_kind}")))
                .example(Some(cursor)),
        )
        .parameter(
            ParameterBuilder::new()
                .name("limit")
                .parameter_in(ParameterIn::Query)
                .description(Some("Rows per page, 1 to 100"))
                .example(Some(json!(25))),
        )
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response("200", json_response("The bounded health view", example))
}

fn json_response(description: &str, example: serde_json::Value) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content("application/json", ContentBuilder::new().example(Some(example)).build())
}

#[cfg(test)]
#[path = "../tests/unit/openapi_tests.rs"]
mod tests;
