pub(super) use peryx_driver::openapi::{
    api_json_response, bounded_integer_parameter, enum_parameter, parameter, route_param, string_array_parameter,
    text_response,
};
pub(super) use serde_json::json;
pub(super) use utoipa::openapi::content::ContentBuilder;
pub(super) use utoipa::openapi::path::{OperationBuilder, ParameterBuilder, ParameterIn};
pub(super) use utoipa::openapi::request_body::RequestBodyBuilder;
pub(super) use utoipa::openapi::{Required, ResponseBuilder, SecurityRequirement};

pub(super) const MIME_SIMPLE_JSON: &str = "application/vnd.pypi.simple.v1+json";
pub(super) fn project_param() -> ParameterBuilder {
    parameter(
        "project",
        ParameterIn::Path,
        "The normalized (PEP 503) project name",
        json!("requests"),
    )
}
pub(super) fn version_param() -> ParameterBuilder {
    parameter("version", ParameterIn::Path, "One release version", json!("1.2.0"))
}
pub(super) fn accept_param() -> ParameterBuilder {
    parameter(
        "Accept",
        ParameterIn::Header,
        "Clients may rank PEP 691 JSON and PEP 503 HTML media ranges with `q` weights",
        json!(MIME_SIMPLE_JSON),
    )
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

/// What the index ACL refuses a read with. Every read route answers these, and says the same thing
/// whether or not the resource exists.
pub(super) fn unauthorized_read_response() -> ResponseBuilder {
    ResponseBuilder::new().description(
        "The index does not allow anonymous reads and the request carried no usable credential; the \
         reply carries `WWW-Authenticate: Basic realm=\"peryx\"`",
    )
}

pub(super) fn forbidden_read_response() -> ResponseBuilder {
    ResponseBuilder::new().description("The presented credential grants no read of this resource")
}

pub(super) fn sha256_param() -> ParameterBuilder {
    parameter(
        "sha256",
        ParameterIn::Path,
        "The artifact's sha256, lowercase hex",
        json!("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"),
    )
}

pub(super) fn range_param() -> ParameterBuilder {
    parameter(
        "Range",
        ParameterIn::Header,
        "One byte range over a cached artifact; multiple ranges are ignored",
        json!("bytes=0-1023"),
    )
}

pub(super) fn if_none_match_param() -> ParameterBuilder {
    parameter(
        "If-None-Match",
        ParameterIn::Header,
        "Entity tags the client already holds; a match answers `304` before any range is read",
        json!("\"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08\""),
    )
}

pub(super) fn if_range_param() -> ParameterBuilder {
    parameter(
        "If-Range",
        ParameterIn::Header,
        "The entity tag the client's partial copy was cut from; the `Range` is served only while \
             it still names the artifact, and the whole artifact otherwise",
        json!("\"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08\""),
    )
}

pub(super) fn if_modified_since_param() -> ParameterBuilder {
    parameter(
        "If-Modified-Since",
        ParameterIn::Header,
        "The `Last-Modified` date of a cached artifact the client already holds; answered `304` \
             unless the store wrote the blob later. Ignored when the request also sends `If-None-Match`",
        json!("Wed, 21 Oct 2026 07:28:00 GMT"),
    )
}

pub(super) fn filename_param(example: &str) -> ParameterBuilder {
    parameter(
        "filename",
        ParameterIn::Path,
        "The display filename, percent-encoded as one path segment. Separators, traversal, and \
             control characters are rejected.",
        json!(example),
    )
}
