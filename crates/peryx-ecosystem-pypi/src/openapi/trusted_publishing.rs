use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::OperationBuilder;
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::{Required, ResponseBuilder};

use peryx_driver::openapi::api_json_response;

pub(super) fn oidc_audience() -> OperationBuilder {
    OperationBuilder::new()
        .tag("trusted publishing")
        .summary(Some("Discover the CI identity audience"))
        .response(
            "200",
            api_json_response("The configured OIDC audience", json!({"audience": "packages.example"})),
        )
        .response("404", ResponseBuilder::new().description("No trusted publisher exists"))
}

pub(super) fn oidc_mint_token() -> OperationBuilder {
    OperationBuilder::new()
        .tag("trusted publishing")
        .summary(Some("Exchange a CI identity for an upload token"))
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
