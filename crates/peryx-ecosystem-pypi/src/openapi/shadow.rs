use serde_json::json;
use utoipa::openapi::path::{OperationBuilder, ParameterIn};
use utoipa::openapi::{Required, ResponseBuilder, SecurityRequirement};

use peryx_driver::openapi::{api_json_response, bounded_integer_parameter, parameter};

fn shadow_example() -> serde_json::Value {
    json!({
        "candidates": [
            {
                "member": "hosted",
                "source": "hosted",
                "filename": "artifact.bin",
                "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "selected": true
            },
            {
                "member": "example",
                "source": "cached",
                "filename": "artifact.bin",
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
            json!("root/pypi"),
        ),
        (
            "project",
            true,
            "The project to explain, normalized per the Python package name rules",
            json!("acme-pkg"),
        ),
        (
            "cursor",
            false,
            "Exclusive cursor from the prior page",
            json!("artifact.bin\u{1f}0\u{1f}hosted"),
        ),
    ] {
        operation = operation.parameter(
            parameter(name, ParameterIn::Query, description, example)
                .required(if required { Required::True } else { Required::False })
                .build(),
        );
    }
    operation.parameter(bounded_integer_parameter(
        "limit",
        ParameterIn::Query,
        "Rows to return, from 1 through 100; defaults to 25",
        json!(25),
        Some(1),
        Some(100),
    ))
}

pub(super) fn shadow_candidates() -> OperationBuilder {
    shadow_parameters(
        OperationBuilder::new()
            .tag("operations")
            .summary(Some("Virtual repository shadowed candidates"))
            .description(Some(
                "Explains candidate selection for one subject in a virtual repository. Each row identifies \
                 its member, source, digest, and rejection reason: `precedence` when another member supplied \
                 the artifact first, `fallback` when the fallback mode excluded a cached member, or \
                 `protected-name` when the subject is protected from upstream fallback. A recorded policy \
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
                    "A parameter is missing, unknown, duplicated, or invalid",
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
