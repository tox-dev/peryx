//! Shared parameter and response builders used across every ecosystem's operations.

use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{ParameterBuilder, ParameterIn};
use utoipa::openapi::{Required, ResponseBuilder};

pub(super) const MIME_SIMPLE_JSON: &str = "application/vnd.pypi.simple.v1+json";

pub(super) fn route_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("route")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The index route, for example `root/pypi`"))
        .example(Some(json!("root/pypi")))
}

pub(super) fn query_param(
    name: &'static str,
    description: &'static str,
    example: serde_json::Value,
) -> ParameterBuilder {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(ParameterIn::Query)
        .description(Some(description))
        .example(Some(example))
}

pub(super) fn api_json_response(description: &str, example: serde_json::Value) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content("application/json", ContentBuilder::new().example(Some(example)).build())
}

pub(super) fn text_response(description: &str, content_type: &str, example: &str) -> ResponseBuilder {
    ResponseBuilder::new().description(description).content(
        content_type,
        ContentBuilder::new().example(Some(json!(example))).build(),
    )
}
