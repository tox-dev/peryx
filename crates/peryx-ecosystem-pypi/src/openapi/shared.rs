//! The parameters, responses, media type and shared builders the `PyPI` operations use.

pub(super) use peryx_driver::openapi::{api_json_response, route_param, text_response};
pub(super) use serde_json::json;
pub(super) use utoipa::openapi::content::ContentBuilder;
pub(super) use utoipa::openapi::path::{OperationBuilder, ParameterBuilder, ParameterIn};
pub(super) use utoipa::openapi::request_body::RequestBodyBuilder;
pub(super) use utoipa::openapi::{Required, ResponseBuilder, SecurityRequirement};

pub(super) const MIME_SIMPLE_JSON: &str = "application/vnd.pypi.simple.v1+json";
pub(super) fn project_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("project")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The normalized (PEP 503) project name"))
        .example(Some(json!("requests")))
}
pub(super) fn version_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("version")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("One release version"))
        .example(Some(json!("1.2.0")))
}
pub(super) fn accept_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("Accept")
        .parameter_in(ParameterIn::Header)
        .description(Some(
            "Clients may rank PEP 691 JSON and PEP 503 HTML media ranges with `q` weights",
        ))
        .example(Some(json!(MIME_SIMPLE_JSON)))
}
pub(super) fn json_response(description: &str, example: serde_json::Value) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content(MIME_SIMPLE_JSON, ContentBuilder::new().example(Some(example)).build())
}
pub(super) fn policy_denial_response(description: &str, action: &str) -> ResponseBuilder {
    api_json_response(
        description,
        json!({
            "action": action,
            "project": "flask",
            "filename": "flask-1.0-py3-none-any.whl",
            "version": "1.0",
            "rule": "max-file-size",
            "field": "size",
            "reason": "file size 2048 exceeds limit 1024"
        }),
    )
}

pub(super) fn sha256_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("sha256")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The artifact's sha256, lowercase hex"))
        .example(Some(json!(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        )))
}

pub(super) fn filename_param(example: &str) -> ParameterBuilder {
    ParameterBuilder::new()
        .name("filename")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some(
            "The display filename, percent-encoded as one path segment. Separators, traversal, and \
             control characters are rejected.",
        ))
        .example(Some(json!(example)))
}
