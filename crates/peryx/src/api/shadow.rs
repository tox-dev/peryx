use serde_json::json;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::{PathsBuilder, Required, ResponseBuilder, SecurityRequirement};

use peryx_driver::openapi::api_json_response;

pub(super) fn shadow_paths(paths: PathsBuilder) -> PathsBuilder {
    paths.path(
        "/+shadow/candidates",
        PathItemBuilder::new()
            .operation(HttpMethod::Get, shadow_candidates())
            .build(),
    )
}

fn shadow_example() -> serde_json::Value {
    json!({
        "candidates": [
            {
                "member": "hosted",
                "source": "hosted",
                "artifact": "artifact.bin",
                "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "selected": true
            },
            {
                "member": "example",
                "source": "cached",
                "artifact": "artifact.bin",
                "digest": "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
                "selected": false,
                "reason": "precedence",
                "decision": {
                    "state": "deny",
                    "rule": "blocked-subject",
                    "reason": "subject is blocked by policy",
                    "evaluated_at_unix": 1_700_000_000,
                    "fresh": true
                }
            }
        ],
        "next_cursor": null
    })
}

fn shadow_parameters(mut operation: OperationBuilder) -> OperationBuilder {
    for (name, required, description, example) in [
        (
            "repository",
            true,
            "The virtual repository route to inspect",
            json!("root/artifacts"),
        ),
        (
            "resource",
            true,
            "The resource to explain, normalized to the ecosystem's canonical form",
            json!("example"),
        ),
        (
            "cursor",
            false,
            "Exclusive cursor from the prior page",
            json!("artifact.bin\u{1f}0\u{1f}hosted"),
        ),
        (
            "limit",
            false,
            "Rows to return, from 1 through 100; defaults to 25",
            json!(25),
        ),
    ] {
        operation = operation.parameter(
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Query)
                .required(if required { Required::True } else { Required::False })
                .description(Some(description))
                .example(Some(example)),
        );
    }
    operation
}

fn shadow_candidates() -> OperationBuilder {
    shadow_parameters(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Virtual repository shadowed candidates"))
            .description(Some(
                "Explains candidate selection for one subject in a virtual repository. Each row identifies \
                 its member, source, digest, and rejection reason: `precedence` when another member supplied \
                 the artifact first, or `fallback` when policy excluded a cached member. A recorded policy \
                 decision includes its state, matched rule, sanitized reason, and retry time. Repository \
                 readers and authorized ecosystem credentials can inspect the repository. Server operators \
                 without repository access cannot. The query reads stored records without changing selection.",
            ))
            .security(SecurityRequirement::new("writeToken", Vec::<String>::new()))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .response(
                "200",
                api_json_response(
                    "The selected and shadowed candidates, artifacts ascending and the selected candidate first",
                    shadow_example(),
                ),
            )
            .response(
                "400",
                api_json_response(
                    "The limit, cursor, or resource is invalid, or a required parameter is missing",
                    json!({"error": "limit must be between 1 and 100"}),
                ),
            )
            .response(
                "401",
                ResponseBuilder::new().description("No valid local or ecosystem credential was presented"),
            )
            .response(
                "403",
                ResponseBuilder::new().description("The credential cannot inspect shadowed candidates"),
            )
            .response(
                "404",
                ResponseBuilder::new()
                    .description("The repository does not exist or is not available to the local user"),
            )
            .response(
                "500",
                api_json_response(
                    "A member's resolution scan could not complete",
                    json!({"error": "shadow query failed"}),
                ),
            )
            .response(
                "503",
                api_json_response(
                    "Authentication or authorization storage is unavailable",
                    json!({"error": "shadow inspection service unavailable"}),
                ),
            ),
    )
}
