//! The `OpenAPI` description of the OCI distribution-spec `/v2/` routes and peryx's layer browser.

use serde_json::json;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::{PathsBuilder, Required, ResponseBuilder, SecurityRequirement};

use peryx_http::openapi::{api_json_response, query_param};

/// The OCI distribution-spec `/v2/` routes an OCI index serves, plus peryx's own layer browser. The
/// composition root folds each ecosystem's paths into one document.
#[must_use]
pub fn openapi_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/v2/",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_version_check())
                .build(),
        )
        .path(
            "/v2/{name}/manifests/{reference}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_manifest_pull())
                .operation(HttpMethod::Put, oci_manifest_push())
                .operation(HttpMethod::Delete, oci_manifest_delete())
                .build(),
        )
        .path(
            "/v2/{name}/blobs/{digest}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_blob_pull())
                .operation(HttpMethod::Delete, oci_blob_delete())
                .build(),
        )
        .path(
            "/v2/{name}/blobs/{digest}/contents",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_layer_contents())
                .build(),
        )
        .path(
            "/v2/{name}/blobs/uploads/",
            PathItemBuilder::new()
                .operation(HttpMethod::Post, oci_blob_upload_start())
                .build(),
        )
        .path(
            "/v2/{name}/blobs/uploads/{session}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_blob_upload_status())
                .operation(HttpMethod::Patch, oci_blob_upload_chunk())
                .operation(HttpMethod::Put, oci_blob_upload_finish())
                .build(),
        )
        .path(
            "/v2/{name}/tags/list",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_tags_list())
                .build(),
        )
        .path(
            "/v2/{name}/referrers/{digest}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_referrers())
                .build(),
        )
}

fn name_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("name")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The repository name, carrying the OCI index route as a prefix"))
        .example(Some(json!("dockerhub/library/alpine")))
}

fn reference_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("reference")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("A tag or an `algorithm:hex` digest"))
        .example(Some(json!("latest")))
}

fn digest_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("digest")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("A content digest; blob digests must be `sha256:...`"))
        .example(Some(json!("sha256:2c3e...")))
}

fn session_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("session")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("An in-progress upload session id"))
        .example(Some(json!("0000000000000000000000000000abcd")))
}

fn oci_version_check() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Registry version check"))
        .description(Some(
            "Confirms this is an OCI distribution-spec registry. Answers `200` with \
             `Docker-Distribution-API-Version: registry/2.0` and an empty body.",
        ))
        .response("200", ResponseBuilder::new().description("A `/v2/` registry"))
}

fn oci_manifest_pull() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Pull a manifest"))
        .description(Some(
            "Resolves the reference hosted-first through the index's members and serves the manifest, \
             pulling it through an online proxy member on a miss.",
        ))
        .parameter(name_param())
        .parameter(reference_param())
        .response("200", ResponseBuilder::new().description("The manifest bytes"))
        .response("404", ResponseBuilder::new().description("`MANIFEST_UNKNOWN`"))
}

fn oci_manifest_push() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Push a manifest"))
        .description(Some(
            "Stores the manifest under its canonical `sha256:` digest and, for a tag reference, points \
             the tag at it. Requires a writable hosted index and its upload token.",
        ))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .parameter(name_param())
        .parameter(reference_param())
        .response("201", ResponseBuilder::new().description("Stored"))
        .response(
            "403",
            ResponseBuilder::new().description("`DENIED`: read-only index or blocked by policy"),
        )
}

fn oci_manifest_delete() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Delete a manifest or untag"))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .parameter(name_param())
        .parameter(reference_param())
        .response("202", ResponseBuilder::new().description("Removed"))
        .response("404", ResponseBuilder::new().description("`MANIFEST_UNKNOWN`"))
}

fn oci_blob_pull() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Pull a blob"))
        .description(Some(
            "Serves the blob from the content-addressed store, pulling it through an online proxy member \
             on a miss; concurrent misses share one upstream fetch. Range-capable.",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .response("200", ResponseBuilder::new().description("The blob bytes"))
        .response("206", ResponseBuilder::new().description("A requested byte range"))
        .response("404", ResponseBuilder::new().description("`BLOB_UNKNOWN`"))
}

fn oci_blob_delete() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Delete a blob"))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .parameter(name_param())
        .parameter(digest_param())
        .response("202", ResponseBuilder::new().description("Removed"))
}

fn oci_layer_contents() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Browse a layer's files"))
        .description(Some(
            "peryx's own layer browser (not a distribution-spec route): lists a stored layer's tar \
             members, or with `?member=` previews one text member in bounded chunks.",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .parameter(query_param(
            "member",
            "A member path to preview",
            json!("etc/os-release"),
        ))
        .response(
            "200",
            ResponseBuilder::new().description("The member list, or one member's text"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The blob or member is unknown"),
        )
}

fn oci_blob_upload_start() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Begin, mount, or monolithically push a blob"))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .parameter(name_param())
        .parameter(query_param(
            "digest",
            "Monolithic push: the blob digest",
            json!("sha256:2c3e..."),
        ))
        .parameter(query_param(
            "mount",
            "Cross-repo mount: an already-stored digest",
            json!("sha256:2c3e..."),
        ))
        .response(
            "201",
            ResponseBuilder::new().description("Mounted or monolithically stored"),
        )
        .response(
            "202",
            ResponseBuilder::new().description("A session opened for chunked upload"),
        )
}

fn oci_blob_upload_status() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Report upload progress"))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .parameter(name_param())
        .parameter(session_param())
        .response("204", ResponseBuilder::new().description("The bytes received so far"))
}

fn oci_blob_upload_chunk() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Append a chunk"))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .parameter(name_param())
        .parameter(session_param())
        .response("202", ResponseBuilder::new().description("Appended"))
        .response("416", ResponseBuilder::new().description("The chunk is out of order"))
}

fn oci_blob_upload_finish() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Finish an upload"))
        .security(SecurityRequirement::new("uploadToken", Vec::<String>::new()))
        .parameter(name_param())
        .parameter(session_param())
        .parameter(query_param("digest", "The full blob digest", json!("sha256:2c3e...")))
        .response("201", ResponseBuilder::new().description("Committed"))
        .response(
            "403",
            ResponseBuilder::new().description("`DENIED`: over the index's size limit"),
        )
}

fn oci_tags_list() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("List tags"))
        .description(Some(
            "Answers `{\"name\", \"tags\"}`, with `n`/`last` pagination and a `Link` next-page header.",
        ))
        .parameter(name_param())
        .parameter(query_param("n", "Page size", json!(50)))
        .parameter(query_param("last", "The tag to resume after", json!("1.0")))
        .response(
            "200",
            api_json_response(
                "The tag list",
                json!({"name": "library/alpine", "tags": ["3.19", "latest"]}),
            ),
        )
}

fn oci_referrers() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("List referrers"))
        .description(Some(
            "The manifests that declare `{digest}` as their subject (attestations, signatures).",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .response("200", ResponseBuilder::new().description("An image-index of referrers"))
}
