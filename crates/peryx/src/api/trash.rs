use serde_json::json;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::{PathsBuilder, Required, ResponseBuilder, SecurityRequirement};

use peryx_driver::openapi::api_json_response;

pub(super) fn trash_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+trash",
            PathItemBuilder::new().operation(HttpMethod::Get, list_trash()).build(),
        )
        .path(
            "/+trash/record",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, inspect_trash())
                .build(),
        )
}

fn trash_record_example() -> serde_json::Value {
    json!({
        "ecosystem": "example",
        "repository": "hosted",
        "name": "example",
        "reference": "example-1.0.bin",
        "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "reason": "compromised build",
        "actor": "usr_550e8400e29b41d4a716446655440000",
        "deleted_at_unix": 1_800_000_000,
        "deadline_unix": 1_802_592_000,
        "state": "restorable",
        "restorable": true
    })
}

fn list_trash() -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("operations")
        .summary(Some("List trashed artifacts"))
        .description(Some(
            "Soft-deleted artifacts across repositories, newest first. Administrators can inspect every \
             repository and see actor details. Repository readers and authorized ecosystem credentials \
             can inspect one repository; role filtering redacts actor details. `restorable` and `state` \
             reflect content retention and the recovery deadline. Responses exclude credentials and \
             request headers and use `no-store`.",
        ))
        .security(SecurityRequirement::new("writeToken", Vec::<String>::new()))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "200",
            api_json_response(
                "The matching trash records, newest first",
                json!({"trash": [trash_record_example()], "next_cursor": "0009223372036800000\u{1f}example\u{1f}hosted\u{1f}example\u{1f}example-1.0.bin\u{1f}sha256:0123"}),
            ),
        )
        .response(
            "400",
            api_json_response(
                "The limit, cursor, or a filter is invalid",
                json!({"error": "limit must be between 1 and 100"}),
            ),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid local or ecosystem credential was presented"),
        )
        .response(
            "403",
            ResponseBuilder::new().description("The credential cannot inspect trash"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The repository does not exist or is not available to the local user"),
        )
        .response(
            "500",
            api_json_response(
                "An ecosystem trash scan could not complete",
                json!({"error": "trash query failed"}),
            ),
        )
        .response(
            "503",
            api_json_response(
                "Authentication or authorization storage is unavailable",
                json!({"error": "trash inspection service unavailable"}),
            ),
        );
    for (name, description, example) in [
        (
            "repository",
            "Repository route to inspect, at most 512 bytes",
            json!("hosted"),
        ),
        ("ecosystem", "Filter by registered ecosystem", json!("example")),
        ("state", "Filter by `restorable` or `expired`", json!("restorable")),
        (
            "deadline_before",
            "Keep records whose recovery deadline is at or before this Unix time",
            json!(1_802_592_000),
        ),
        (
            "cursor",
            "Exclusive cursor from the prior page",
            json!("0009223372036800000\u{1f}example\u{1f}hosted\u{1f}example\u{1f}example-1.0.bin\u{1f}sha256:0123"),
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

fn inspect_trash() -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("operations")
        .summary(Some("Inspect one trashed artifact"))
        .description(Some(
            "One soft-deleted artifact identified by its ecosystem, repository, and name, with the same \
             role-filtered actor visibility as the list. Returns the current derived state without \
             changing lifecycle, retention, or policy state.",
        ))
        .security(SecurityRequirement::new("writeToken", Vec::<String>::new()))
        .security(SecurityRequirement::new("administratorPassword", Vec::<String>::new()))
        .response(
            "200",
            api_json_response("The matching trash record", json!({"record": trash_record_example()})),
        )
        .response(
            "400",
            api_json_response("The ecosystem is unknown", json!({"error": "invalid trash query"})),
        )
        .response(
            "401",
            ResponseBuilder::new().description("No valid local or ecosystem credential was presented"),
        )
        .response(
            "403",
            ResponseBuilder::new().description("The credential cannot inspect trash"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("No such repository or trashed record is available to the caller"),
        );
    for (name, required, description, example) in [
        (
            "ecosystem",
            true,
            "The artifact's registered ecosystem",
            json!("example"),
        ),
        (
            "repository",
            true,
            "The repository route the artifact was deleted from",
            json!("hosted"),
        ),
        ("name", true, "The ecosystem artifact name", json!("example")),
        (
            "reference",
            false,
            "The ecosystem artifact reference, if any",
            json!("example-1.0.bin"),
        ),
        (
            "digest",
            false,
            "The content digest, if the ecosystem addresses the artifact by one",
            json!("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
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
