//! The `OpenAPI` description of the operator shadowed-candidate endpoint.

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
                "filename": "example-1.0.bin",
                "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "selected": true
            },
            {
                "member": "example",
                "source": "cached",
                "filename": "example-1.0.bin",
                "digest": "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
                "selected": false,
                "reason": "precedence",
                "decision": {
                    "state": "deny",
                    "rule": "blocked-project",
                    "reason": "project is blocked by policy",
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
            json!("root/packages"),
        ),
        (
            "project",
            true,
            "The project to explain, normalized to the ecosystem's canonical form",
            json!("example"),
        ),
        (
            "cursor",
            false,
            "Exclusive cursor from the prior page",
            json!("example-1.0.bin\u{1f}0\u{1f}hosted"),
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
                "Explains how a virtual repository resolves one project: the selected candidate for each \
                 distribution filename and every candidate a member shadowed, with its configured member, \
                 source class, digest, and the reason it lost - `precedence` when a higher-precedence \
                 member already supplied the filename, or `fallback` when the repository's fallback policy \
                 excluded a cached member. Each candidate also carries the recorded policy decision that \
                 governs its filename when one exists - `allow`, `deny`, or `wait`, with the matched rule, \
                 a reason already stripped of any upstream URL or credential, and a retry time for a wait - \
                 so an operator sees blocked and held candidates beside the shadowed ones. A caller who can \
                 read the repository may inspect it; the server operator role, which carries no repository \
                 access, cannot. A repository's legacy upload token retains access under the `__token__` \
                 username. The query reads stored records only and never changes member order, installer \
                 responses, or policy evaluation, so shadowed candidates stay absent from HTML and JSON \
                 installer selection.",
            ))
            .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
            .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
            .response(
                "200",
                api_json_response(
                    "The selected and shadowed candidates, filenames ascending and the selected candidate first",
                    shadow_example(),
                ),
            )
            .response(
                "400",
                api_json_response(
                    "The limit, cursor, or project is invalid, or a required parameter is missing",
                    json!({"error": "limit must be between 1 and 100"}),
                ),
            )
            .response(
                "401",
                ResponseBuilder::new().description("No valid local user credential or repository token was presented"),
            )
            .response(
                "403",
                ResponseBuilder::new().description("The repository token cannot inspect shadowed candidates"),
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
