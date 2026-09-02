//! The `OpenAPI` description of the OCI distribution-spec `/v2/` routes and peryx's own extensions.
//!
//! Every path and method here is one the registry dispatcher serves. The inventory test in
//! `tests/unit/tests/openapi_tests.rs` compares this document against the `Allow` header each route
//! answers, so the two cannot drift.

use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::header::{Header, HeaderBuilder};
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::schema::{KnownFormat, ObjectBuilder, SchemaFormat, Type};
use utoipa::openapi::{PathsBuilder, Required, ResponseBuilder};

use peryx_driver::openapi::{api_json_response, bounded_integer_parameter, parameter, query_param};
use peryx_driver::route_auth::{ReadExposure, RouteAuth};

/// The OCI distribution-spec `/v2/` routes an OCI index serves, plus peryx's own restore and layer
/// browser. The composition root folds each ecosystem's paths into one document.
#[must_use]
pub fn openapi_paths(paths: PathsBuilder, reads: ReadExposure) -> PathsBuilder {
    paths
        .path(
            "/v2/",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_version_check(reads))
                .operation(HttpMethod::Head, oci_version_head(reads))
                .build(),
        )
        .path(
            "/v2/token",
            PathItemBuilder::new().operation(HttpMethod::Get, oci_token()).build(),
        )
        .path(
            "/v2/_catalog",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_catalog(reads))
                .build(),
        )
        .path(
            "/v2/{name}/manifests/{reference}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_manifest_pull(reads))
                .operation(HttpMethod::Head, oci_manifest_head(reads))
                .operation(HttpMethod::Put, oci_manifest_push())
                .operation(HttpMethod::Delete, oci_manifest_delete())
                .build(),
        )
        .path(
            "/v2/{name}/manifests/{reference}/restore",
            PathItemBuilder::new()
                .operation(HttpMethod::Put, oci_manifest_restore())
                .build(),
        )
        .path(
            "/v2/{name}/blobs/{digest}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_blob_pull(reads))
                .operation(HttpMethod::Head, oci_blob_head(reads))
                .operation(HttpMethod::Delete, oci_blob_delete())
                .build(),
        )
        .path(
            "/v2/{name}/blobs/{digest}/contents",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_layer_contents(reads))
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
                .operation(HttpMethod::Delete, oci_blob_upload_cancel())
                .build(),
        )
        .path(
            "/v2/{name}/tags/list",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_tags_list(reads))
                .build(),
        )
        .path(
            "/v2/{name}/referrers/{digest}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, oci_referrers(reads))
                .operation(HttpMethod::Head, oci_referrers_head(reads))
                .build(),
        )
}

fn name_param() -> ParameterBuilder {
    parameter(
        "name",
        ParameterIn::Path,
        "The repository name, carrying the OCI index route as a prefix",
        json!("dockerhub/library/alpine"),
    )
}

fn reference_param() -> ParameterBuilder {
    parameter(
        "reference",
        ParameterIn::Path,
        "A tag or an `algorithm:hex` digest",
        json!("latest"),
    )
}

fn digest_param() -> ParameterBuilder {
    parameter(
        "digest",
        ParameterIn::Path,
        "A content digest; blob digests must be `sha256:...`",
        json!("sha256:2c3e..."),
    )
}

fn if_none_match_param() -> ParameterBuilder {
    parameter(
        "If-None-Match",
        ParameterIn::Header,
        "Entity tags the client already holds; a match answers `304` before any body or range is read",
        json!("\"sha256:2c3e...\""),
    )
}

fn session_param() -> ParameterBuilder {
    parameter(
        "session",
        ParameterIn::Path,
        "An in-progress upload session id",
        json!("0000000000000000000000000000abcd"),
    )
}

fn content_range_param() -> ParameterBuilder {
    parameter(
        "Content-Range",
        ParameterIn::Header,
        "The inclusive `<start>-<end>` this chunk covers, optionally prefixed `bytes `. \
         It must begin where the last chunk ended and span exactly the bytes the body carries; \
         omit it to append wherever the session stands.",
        json!("0-1023"),
    )
}

/// The raw blob bytes a monolithic push, a chunk, or a closing `PUT` carries.
fn blob_body(description: &'static str) -> RequestBodyBuilder {
    RequestBodyBuilder::new().description(Some(description)).content(
        "application/octet-stream",
        ContentBuilder::new()
            .schema(Some(
                ObjectBuilder::new()
                    .schema_type(Type::String)
                    .format(Some(SchemaFormat::KnownFormat(KnownFormat::Binary))),
            ))
            .build(),
    )
}

/// A response header a client reads to drive the protocol, described for a generated client.
fn header(description: &'static str) -> Header {
    HeaderBuilder::new().description(Some(description)).build()
}

/// The distribution-spec error envelope, whose `code` a client switches on.
fn oci_error(description: &str, code: &str, message: &str) -> ResponseBuilder {
    api_json_response(description, json!({"errors": [{"code": code, "message": message}]}))
}

/// The `401` challenge that starts the token handshake. `scope` names the token to ask `/v2/token`
/// for, and `error` distinguishes a rejected token from a missing one.
fn oci_challenge(description: &str, scope: &str) -> ResponseBuilder {
    oci_error(description, "UNAUTHORIZED", "authentication required").header(
        "WWW-Authenticate",
        header(match scope {
            "" => {
                "`Bearer realm=\"<base>/v2/token\",service=\"peryx\"`, or `Basic realm=\"peryx\"` \
                   when no token realm runs"
            }
            _ => {
                "`Bearer realm=\"<base>/v2/token\",service=\"peryx\",scope=\"<scope>\"`, optionally \
                  with `error=\"invalid_token\"` or `error=\"insufficient_scope\"`"
            }
        }),
    )
}

fn oci_version_check(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("An OCI index restricts access", ""))
        .tag("oci")
        .summary(Some("Registry version check"))
        .description(Some(
            "Confirms this is an OCI distribution-spec registry (spec end-1). Answers `200` with \
             `Docker-Distribution-API-Version: registry/2.0` and an empty body. When an OCI index \
             restricts access and the request carries no accepted credential it answers the `401` \
             challenge `docker login` starts from.",
        ))
        .response(
            "200",
            ResponseBuilder::new()
                .description("Registry API capability response")
                .header("Docker-Distribution-API-Version", header("Always `registry/2.0`")),
        )
}

fn oci_version_head(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("An OCI index restricts access", ""))
        .tag("oci")
        .summary(Some("Registry version check without a body"))
        .description(Some("The version check's headers alone. Same statuses as the `GET`."))
        .response(
            "200",
            ResponseBuilder::new()
                .description("Registry API capability response")
                .header("Docker-Distribution-API-Version", header("Always `registry/2.0`")),
        )
}

fn oci_token() -> OperationBuilder {
    OperationBuilder::new()
        .tag("oci")
        .summary(Some("Mint a scoped bearer token"))
        .description(Some(
            "The Docker token-authentication handshake a `401` challenge sends a client to. \
             `Authorization: Basic` names the requester; without it the token carries the grants an \
             anonymous puller already has. A scope that resolves to no index, authenticates as \
             another subject, or grants nothing contributes no grant, so the minted token is always \
             a subset of what the requester may already do. The response is never cached: it carries \
             `Cache-Control: no-store` and `Pragma: no-cache`.",
        ))
        .parameter(
            query_param(
                "service",
                "The realm's audience; a value that does not match it is refused",
                json!("peryx"),
            )
            .required(Required::True),
        )
        .parameter(query_param(
            "scope",
            "A space-separated list of `repository:<name>:pull,push,delete` or `registry:catalog:*` \
             scopes. The parameter may repeat, and every occurrence is read.",
            json!("repository:dockerhub/library/alpine:pull"),
        ))
        .response(
            "200",
            api_json_response(
                "A signed bearer token and its lifetime in seconds",
                json!({"token": "eyJhbGci...", "access_token": "eyJhbGci...", "expires_in": 3600}),
            )
            .header("Cache-Control", header("Always `no-store`")),
        )
        .response(
            "401",
            oci_error(
                "The `Authorization: Basic` credentials name no known subject",
                "UNAUTHORIZED",
                "invalid credentials",
            ),
        )
        .response(
            "403",
            oci_error(
                "`service` is absent, repeated, or names another realm",
                "DENIED",
                "requested service is not available",
            ),
        )
        .response(
            "404",
            oci_error(
                "No token realm is configured",
                "NAME_UNKNOWN",
                "repository name unknown",
            ),
        )
}

fn oci_catalog(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge(
            "A private OCI index is configured and the request carries no `registry:catalog:*` grant",
            "registry:catalog:*",
        ))
        .tag("oci")
        .summary(Some("List repositories"))
        .description(Some(
            "The union of every OCI index's repositories as clients address them: each entry is the \
             index route joined to the repository, so a listed name is one a client can pull. The set \
             is sorted, a serve-policy rule omits the repositories it blocks, and `n`/`last` paginate \
             it exactly as `tags/list` does.",
        ))
        .parameter(bounded_integer_parameter(
            "n",
            ParameterIn::Query,
            "Page size; `0` answers an empty list with no `Link`",
            json!(50),
            Some(0),
            None,
        ))
        .parameter(query_param(
            "last",
            "The repository to resume after",
            json!("images/api"),
        ))
        .response(
            "200",
            api_json_response(
                "The repository list",
                json!({"repositories": ["dockerhub/library/alpine", "images/api"]}),
            )
            .header(
                "Link",
                header("`</v2/_catalog?n=<n>&last=<marker>>; rel=\"next\"`, present only when more remains"),
            ),
        )
}

fn oci_manifest_pull(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("The index refuses this read", "repository:<name>:pull"))
        .tag("oci")
        .summary(Some("Pull a manifest"))
        .description(Some(
            "Resolves the reference hosted-first through the index's members and serves the manifest \
             (spec end-3), pulling it through an online proxy member on a miss. The negotiated \
             manifest's quoted digest is the `ETag`, so a client can revalidate the document it holds. \
             When the resolved manifest is an index and `Accept` names neither list media type, peryx \
             serves the index's `linux/amd64` child instead, which is what lets legacy tooling pull.",
        ))
        .parameter(name_param())
        .parameter(reference_param())
        .parameter(parameter(
            "Accept",
            ParameterIn::Header,
            "The manifest media types the client understands; every field line is combined into one list",
            json!("application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json"),
        ))
        .parameter(if_none_match_param())
        .response("200", manifest_body_response("Negotiated manifest body"))
        .response("304", manifest_not_modified_response())
        .response(
            "400",
            oci_error(
                "The reference is a digest peryx cannot key on",
                "DIGEST_INVALID",
                "manifest digest is invalid",
            ),
        )
        .response(
            "404",
            oci_error(
                "No member can serve the reference",
                "MANIFEST_UNKNOWN",
                "manifest unknown",
            ),
        )
}

fn oci_manifest_head(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("The index refuses this read", "repository:<name>:pull"))
        .tag("oci")
        .summary(Some("Check a manifest"))
        .description(Some(
            "The manifest a `GET` would serve, resolved and negotiated identically, with the headers \
             a client needs to decide whether to pull and no body (spec end-3).",
        ))
        .parameter(name_param())
        .parameter(reference_param())
        .parameter(parameter(
            "Accept",
            ParameterIn::Header,
            "The manifest media types the client understands; every field line is combined into one list",
            json!("application/vnd.oci.image.index.v1+json"),
        ))
        .parameter(if_none_match_param())
        .response("200", manifest_body_response("The negotiated manifest's headers"))
        .response("304", manifest_not_modified_response())
        .response(
            "400",
            oci_error(
                "The reference is a digest peryx cannot key on",
                "DIGEST_INVALID",
                "manifest digest is invalid",
            ),
        )
        .response(
            "404",
            oci_error(
                "No member can serve the reference",
                "MANIFEST_UNKNOWN",
                "manifest unknown",
            ),
        )
}

fn manifest_body_response(description: &str) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content(
            "application/vnd.oci.image.manifest.v1+json",
            ContentBuilder::new()
                .example(Some(json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "config": {
                        "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": "sha256:2c3e...",
                        "size": 1469,
                    },
                    "layers": [{
                        "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "digest": "sha256:9f86...",
                        "size": 3_401_327,
                    }],
                })))
                .build(),
        )
        .header(
            "Docker-Content-Digest",
            header("The digest of the exact bytes served, which a client verifies"),
        )
        .header("ETag", header("The same digest, quoted"))
        .header(
            "Content-Length",
            header("The manifest's byte length, set on a `HEAD` too"),
        )
        .header("Vary", header("Always `Accept`"))
}

/// The `304` a matched `If-None-Match` answers, carrying the validators the `200` would have.
fn manifest_not_modified_response() -> ResponseBuilder {
    ResponseBuilder::new()
        .description("The client already holds the negotiated manifest")
        .header("ETag", header("The quoted digest of the negotiated manifest"))
        .header("Docker-Content-Digest", header("The negotiated manifest's digest"))
        .header("Vary", header("Always `Accept`"))
}

fn oci_manifest_push() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials",
            "repository:<name>:pull,push",
        ))
        .tag("oci")
        .summary(Some("Push a manifest"))
        .description(Some(
            "Stores the manifest byte for byte under its canonical `sha256:` digest and, for a tag \
             reference, points the tag at it (spec end-7). The body must parse as the document its \
             `Content-Type` declares and may name only blobs and child manifests this repository can \
             already serve. Requires a writable hosted index and its upload token.",
        ))
        .parameter(name_param())
        .parameter(reference_param())
        .parameter(parameter(
            "Content-Type",
            ParameterIn::Header,
            "The manifest media type to record; parameters after a `;` are ignored. Defaults to \
             `application/vnd.oci.image.manifest.v1+json`.",
            json!("application/vnd.oci.image.manifest.v1+json"),
        ))
        .request_body(Some(
            RequestBodyBuilder::new()
                .required(Some(Required::True))
                .description(Some("The manifest document, at most 4 MiB"))
                .content(
                    "application/vnd.oci.image.manifest.v1+json",
                    ContentBuilder::new()
                        .example(Some(json!({
                            "schemaVersion": 2,
                            "mediaType": "application/vnd.oci.image.manifest.v1+json",
                            "config": {
                                "mediaType": "application/vnd.oci.image.config.v1+json",
                                "digest": "sha256:2c3e...",
                                "size": 1469,
                            },
                            "layers": [{
                                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                                "digest": "sha256:9f86...",
                                "size": 3_401_327,
                            }],
                        })))
                        .build(),
                )
                .build(),
        ))
        .response(
            "201",
            ResponseBuilder::new()
                .description("Stored")
                .header("Location", header("`/v2/<name>/manifests/<canonical digest>`"))
                .header(
                    "Docker-Content-Digest",
                    header("The canonical digest of the stored bytes"),
                )
                .header(
                    "OCI-Subject",
                    header("The subject digest, present only when the manifest declares one"),
                ),
        )
        .response(
            "400",
            oci_error(
                "`DIGEST_INVALID` when the bytes do not hash to a digest reference, `MANIFEST_INVALID` \
                 for an unsupported media type or a body that breaks its schema, `MANIFEST_BLOB_UNKNOWN` \
                 when it names content this repository cannot serve",
                "MANIFEST_BLOB_UNKNOWN",
                "referenced blob sha256:9f86... is not present",
            ),
        )
        .response(
            "403",
            oci_error(
                "Read-only index, uploads disabled, or blocked by policy",
                "DENIED",
                "image name is blocked by policy",
            ),
        )
        .response(
            "413",
            oci_error(
                "The body exceeds the 4 MiB manifest limit",
                "SIZE_INVALID",
                "manifest exceeds the 4194304-byte limit",
            ),
        )
}

fn oci_manifest_delete() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials",
            "repository:<name>:pull,delete",
        ))
        .tag("oci")
        .summary(Some("Trash a manifest or tag"))
        .description(Some(
            "Hides repository metadata and retains the bytes (spec end-9). Deleting a tag hides that \
             tag alone; deleting a digest hides it and every tag in this repository pointing at it. \
             Repositories that share the same content stay visible.",
        ))
        .parameter(name_param())
        .parameter(reference_param())
        .parameter(parameter(
            "reason",
            ParameterIn::Query,
            "Optional deletion reason retained for audit",
            json!("build metadata is incorrect"),
        ))
        .response("202", ResponseBuilder::new().description("Hidden and retained"))
        .response(
            "400",
            oci_error(
                "The reference is a digest peryx cannot key on",
                "DIGEST_INVALID",
                "manifest digest is invalid",
            ),
        )
        .response(
            "403",
            oci_error("Read-only index or uploads disabled", "DENIED", "index is read-only"),
        )
        .response(
            "404",
            oci_error("Absent or already trashed", "MANIFEST_UNKNOWN", "manifest unknown"),
        )
}

fn oci_manifest_restore() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials",
            "repository:<name>:pull,delete",
        ))
        .tag("oci")
        .summary(Some("Restore a trashed manifest or tag"))
        .description(Some(
            "A peryx extension, not a distribution-spec route: makes retained manifest bytes visible \
             again. Digest restore reclaims tags whose live slot is empty and reports reused tags \
             without overwriting them.",
        ))
        .parameter(name_param())
        .parameter(reference_param())
        .response(
            "202",
            ResponseBuilder::new()
                .description("Restored")
                .header("Docker-Content-Digest", header("The restored manifest's digest"))
                .header("OCI-Restored-Tags", header("How many tags the restore reclaimed"))
                .header(
                    "OCI-Tag-Conflicts",
                    header("Comma-separated tags left alone because another digest holds them"),
                ),
        )
        .response(
            "403",
            oci_error("Read-only index or uploads disabled", "DENIED", "index is read-only"),
        )
        .response(
            "404",
            oci_error(
                "Nothing retained under the reference",
                "MANIFEST_UNKNOWN",
                "manifest unknown",
            ),
        )
}

fn oci_blob_pull(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("The index refuses this read", "repository:<name>:pull"))
        .tag("oci")
        .summary(Some("Pull a blob"))
        .description(Some(
            "Serves the blob from the content-addressed store (spec end-2), pulling it through an \
             online proxy member on a miss; concurrent misses share one upstream fetch. Range-capable, \
             with the quoted digest as the `ETag`: a `Range` carrying an `If-Range` is served only \
             while that tag still names the blob, and the whole blob otherwise.",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .parameter(if_none_match_param())
        .parameter(parameter(
            "Range",
            ParameterIn::Header,
            "A single `bytes=<first>-<last>` range; other range units are ignored and the whole blob served",
            json!("bytes=0-1023"),
        ))
        .parameter(parameter(
            "If-Range",
            ParameterIn::Header,
            "The entity tag the client's partial copy was cut from",
            json!("\"sha256:2c3e...\""),
        ))
        .response("200", blob_response("Blob body"))
        .response(
            "206",
            blob_response("A requested byte range").header("Content-Range", header("`bytes <first>-<last>/<size>`")),
        )
        .response("304", blob_not_modified_response())
        .response(
            "400",
            oci_error(
                "The digest names an algorithm the store cannot key on",
                "DIGEST_INVALID",
                "only sha256 blob digests are supported",
            ),
        )
        .response(
            "404",
            oci_error(
                "Neither stored under this repository nor available upstream",
                "BLOB_UNKNOWN",
                "blob unknown",
            ),
        )
        .response(
            "416",
            ResponseBuilder::new()
                .description("The requested range lies outside the blob")
                .header("Content-Range", header("`bytes */<size>`")),
        )
}

fn oci_blob_head(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("The index refuses this read", "repository:<name>:pull"))
        .tag("oci")
        .summary(Some("Check a blob"))
        .description(Some(
            "The size and digest headers a client needs to decide whether to pull, with no body \
             (spec end-2). A `HEAD` transfers no content, so a `Range` never applies (RFC 9110 \
             section 14.2) and an existing blob always reports its full representation size.",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .parameter(if_none_match_param())
        .response("200", blob_response("The blob's headers"))
        .response("304", blob_not_modified_response())
        .response(
            "400",
            oci_error(
                "The digest names an algorithm the store cannot key on",
                "DIGEST_INVALID",
                "only sha256 blob digests are supported",
            ),
        )
        .response(
            "404",
            oci_error(
                "Neither stored under this repository nor available upstream",
                "BLOB_UNKNOWN",
                "blob unknown",
            ),
        )
}

fn blob_response(description: &str) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content("application/octet-stream", ContentBuilder::new().build())
        .header("Docker-Content-Digest", header("The blob's digest"))
        .header("ETag", header("The same digest, quoted"))
        .header(
            "Content-Length",
            header("The bytes this response carries, set on a `HEAD` too"),
        )
        .header("Accept-Ranges", header("Always `bytes`"))
}

/// The `304` for a blob the client already holds: the validators plus the range capability a `200`
/// would have carried, so its next conditional or partial pull has everything it needs.
fn blob_not_modified_response() -> ResponseBuilder {
    ResponseBuilder::new()
        .description("The client already holds the blob")
        .header("ETag", header("The quoted digest"))
        .header("Docker-Content-Digest", header("The blob's digest"))
        .header("Accept-Ranges", header("Always `bytes`"))
}

fn oci_blob_delete() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials",
            "repository:<name>:pull,delete",
        ))
        .tag("oci")
        .summary(Some("Delete a blob"))
        .description(Some(
            "Removes this repository's link to the blob (spec end-10) and leaves the payload in the \
             shared content store for `cache purge orphaned-blobs` to reclaim once no provider \
             references it.",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .response("202", ResponseBuilder::new().description("Removed"))
        .response(
            "400",
            oci_error(
                "The digest names an algorithm the store cannot key on",
                "DIGEST_INVALID",
                "only sha256 blob digests are supported",
            ),
        )
        .response(
            "403",
            oci_error("Read-only index or uploads disabled", "DENIED", "index is read-only"),
        )
        .response(
            "404",
            oci_error("This repository does not link the blob", "BLOB_UNKNOWN", "blob unknown"),
        )
}

fn oci_layer_contents(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("The index refuses this read", "repository:<name>:pull"))
        .tag("oci")
        .summary(Some("Browse a layer's files"))
        .description(Some(
            "A peryx extension, not a distribution-spec route (a plain registry answers `404` here, so \
             it never collides with a pull): lists a stored layer's tar members, or with `?member=` \
             previews one text member in bounded chunks from `?offset=`. A layer missing from the store \
             is fetched once through the single-flight gate first.",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .parameter(query_param(
            "member",
            "A member path to preview; without it the response lists the layer's members",
            json!("etc/os-release"),
        ))
        .parameter(bounded_integer_parameter(
            "offset",
            ParameterIn::Query,
            "The byte offset within `member` to resume the preview from; read back from \
             `x-peryx-next-offset`. Only meaningful alongside `member`.",
            json!(0),
            Some(0),
            None,
        ))
        .response(
            "200",
            api_json_response(
                "The member list",
                json!({"members": [{"path": "etc/os-release", "size": 197, "kind": "text", "previewable": true}]}),
            )
            .content(
                "text/plain",
                ContentBuilder::new()
                    .example(Some(json!("NAME=\"Alpine Linux\"\n")))
                    .build(),
            )
            .header(
                "x-peryx-member-size",
                header("The previewed member's full size in bytes"),
            )
            .header("x-peryx-member-offset", header("Where this chunk starts"))
            .header(
                "x-peryx-next-offset",
                header("Where the next chunk starts, absent on the last one"),
            ),
        )
        .response(
            "400",
            ResponseBuilder::new().description("`offset` is not a non-negative integer"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The blob is unknown, or `member` names no file in the layer"),
        )
        .response(
            "415",
            ResponseBuilder::new().description("`member` names a binary file, which has no text preview"),
        )
        .response(
            "416",
            ResponseBuilder::new().description("`offset` lies past the member's end"),
        )
        .response(
            "422",
            ResponseBuilder::new().description("The blob is not a readable tar, gzip, or uncompressed layer"),
        )
}

fn oci_blob_upload_start() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials, or no pull grant on the `from` repository",
            "repository:<name>:pull,push",
        ))
        .tag("oci")
        .summary(Some("Begin, mount, or monolithically push a blob"))
        .description(Some(
            "Three shapes on one route. `?mount=&from=` cross-repository mounts an already-stored blob \
             without a transfer, once the source repository authorizes the read (spec end-11); a source \
             that lacks the link or the bytes falls through to a session instead of failing. `?digest=` \
             pushes the whole blob in this request body (spec end-4b). A bare `POST` opens a session the \
             client fills with `PATCH` and closes with `PUT` (spec end-4a).",
        ))
        .parameter(name_param())
        .parameter(query_param(
            "digest",
            "Monolithic push: the blob digest the request body must hash to",
            json!("sha256:2c3e..."),
        ))
        .parameter(query_param(
            "mount",
            "Cross-repository mount: a digest already stored under `from`",
            json!("sha256:2c3e..."),
        ))
        .parameter(query_param(
            "from",
            "Cross-repository mount: the source repository name, with its peryx index route. \
             Required alongside `mount`; without it the request opens an ordinary session.",
            json!("dockerhub/library/alpine"),
        ))
        .request_body(Some(
            blob_body("The whole blob, for a monolithic `?digest=` push; empty otherwise").build(),
        ))
        .response(
            "201",
            ResponseBuilder::new()
                .description("Mounted or monolithically stored")
                .header("Location", header("`/v2/<name>/blobs/<digest>`"))
                .header("Docker-Content-Digest", header("The stored blob's digest")),
        )
        .response("202", upload_session_response("A session opened for chunked upload"))
        .response(
            "400",
            oci_error(
                "`?digest=` names an algorithm the store cannot key on, or the body does not hash to it",
                "DIGEST_INVALID",
                "only sha256 blob digests are supported",
            ),
        )
        .response(
            "403",
            oci_error(
                "Read-only index, uploads disabled, blocked by policy, or over the index's size limit",
                "DENIED",
                "image name is blocked by policy",
            ),
        )
}

fn oci_blob_upload_status() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials",
            "repository:<name>:pull,push",
        ))
        .tag("oci")
        .summary(Some("Report upload progress"))
        .description(Some(
            "The bytes the session has received (spec end-13). A status read counts as activity, so it \
             holds the session against the one-hour idle reclamation.",
        ))
        .parameter(name_param())
        .parameter(session_param())
        .response("204", upload_session_response("Current upload offset"))
        .response(
            "403",
            oci_error("Read-only index or uploads disabled", "DENIED", "index is read-only"),
        )
        .response(
            "404",
            oci_error(
                "No open session under this repository carries the id",
                "BLOB_UPLOAD_UNKNOWN",
                "upload unknown",
            ),
        )
}

/// The `Location`, `Docker-Upload-UUID` and `Range` every open-session response carries, so a client
/// always has the coordinates to resume from.
fn upload_session_response(description: &str) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .header("Location", header("`/v2/<name>/blobs/uploads/<session>`"))
        .header("Docker-Upload-UUID", header("The session id"))
        .header(
            "Range",
            header("`0-<offset-1>` for the bytes received so far; a session with none reports `0-0`"),
        )
}

fn oci_blob_upload_chunk() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials",
            "repository:<name>:pull,push",
        ))
        .tag("oci")
        .summary(Some("Append a chunk"))
        .description(Some(
            "Streams the body into the session's durable stage and advances its offset (spec end-5). A \
             mid-body failure leaves the session recorded at the bytes that reached disk, so a client \
             resumes from the reported `Range` rather than re-uploading.",
        ))
        .parameter(name_param())
        .parameter(session_param())
        .parameter(content_range_param())
        .request_body(Some(blob_body("The chunk's bytes").build()))
        .response("202", upload_session_response("Appended"))
        .response(
            "403",
            oci_error(
                "Read-only index, uploads disabled, blocked by policy, or over the index's size limit",
                "DENIED",
                "artifact exceeds the index size limit",
            ),
        )
        .response(
            "404",
            oci_error(
                "No open session under this repository carries the id",
                "BLOB_UPLOAD_UNKNOWN",
                "upload unknown",
            ),
        )
        .response(
            "416",
            upload_session_response(
                "The `Content-Range` does not begin where the last chunk ended, or spans a byte count \
                 the body does not carry. The session keeps its bytes.",
            ),
        )
}

fn oci_blob_upload_finish() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials",
            "repository:<name>:pull,push",
        ))
        .tag("oci")
        .summary(Some("Finish an upload"))
        .description(Some(
            "Appends any trailing body, then verifies the staged bytes against `?digest=` and commits \
             them (spec end-6). The digest is checked before the body is read, so a `PUT` that omits it \
             leaves the stage and offset untouched and the client can retry the same final chunk without \
             appending it twice.",
        ))
        .parameter(name_param())
        .parameter(session_param())
        .parameter(query_param("digest", "The whole blob's digest", json!("sha256:2c3e...")).required(Required::True))
        .parameter(content_range_param())
        .request_body(Some(blob_body("Any trailing bytes; may be empty").build()))
        .response(
            "201",
            ResponseBuilder::new()
                .description("Committed")
                .header("Location", header("`/v2/<name>/blobs/<digest>`"))
                .header("Docker-Content-Digest", header("The committed blob's digest")),
        )
        .response(
            "400",
            oci_error(
                "`digest` is absent, names an algorithm the store cannot key on, or the staged bytes do \
                 not hash to it",
                "DIGEST_INVALID",
                "finishing an upload requires a digest",
            ),
        )
        .response(
            "403",
            oci_error(
                "Read-only index, uploads disabled, or over the index's size limit",
                "DENIED",
                "artifact exceeds the index size limit",
            ),
        )
        .response(
            "404",
            oci_error(
                "No open session under this repository carries the id",
                "BLOB_UPLOAD_UNKNOWN",
                "upload unknown",
            ),
        )
        .response(
            "416",
            upload_session_response(
                "The `Content-Range` does not begin where the last chunk ended, or spans a byte count \
                 the body does not carry",
            ),
        )
}

fn oci_blob_upload_cancel() -> OperationBuilder {
    RouteAuth::Write
        .operation(oci_challenge(
            "Missing or wrong credentials",
            "repository:<name>:pull,push",
        ))
        .tag("oci")
        .summary(Some("Cancel an upload session"))
        .description(Some(
            "Drops the session's durable record and its staged bytes (spec end-14), so a client that \
             abandons a push reclaims the disk immediately instead of waiting for the idle sweep.",
        ))
        .parameter(name_param())
        .parameter(session_param())
        .response("204", ResponseBuilder::new().description("Cancelled"))
        .response(
            "403",
            oci_error("Read-only index or uploads disabled", "DENIED", "index is read-only"),
        )
        .response(
            "404",
            oci_error(
                "No open session under this repository carries the id, including one already committed \
                 or cancelled",
                "BLOB_UPLOAD_UNKNOWN",
                "upload unknown",
            ),
        )
}

fn oci_tags_list(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("The index refuses this read", "repository:<name>:pull"))
        .tag("oci")
        .summary(Some("List tags"))
        .description(Some(
            "Answers `{\"name\", \"tags\"}` (spec end-8a), with `n`/`last` pagination and a `Link` \
             next-page header (spec end-8b). A lone online proxy index passes the upstream response \
             through, rewritten to the client-facing name; every other case unions its members' tags.",
        ))
        .parameter(name_param())
        .parameter(bounded_integer_parameter(
            "n",
            ParameterIn::Query,
            "Page size; `0` answers an empty list with no `Link`",
            json!(50),
            Some(0),
            None,
        ))
        .parameter(query_param("last", "The tag to resume after", json!("1.0")))
        .response(
            "200",
            api_json_response(
                "The tag list",
                json!({"name": "library/alpine", "tags": ["3.19", "latest"]}),
            )
            .header(
                "Link",
                header("`</v2/<name>/tags/list?n=<n>&last=<marker>>; rel=\"next\"`, present only when more remains"),
            ),
        )
        .response(
            "404",
            oci_error(
                "`name` matches no OCI index route",
                "NAME_UNKNOWN",
                "repository name unknown",
            ),
        )
}

fn oci_referrers(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("The index refuses this read", "repository:<name>:pull"))
        .tag("oci")
        .summary(Some("List referrers"))
        .description(Some(
            "The manifests that declare `{digest}` as their subject (attestations, signatures), \
             aggregated across the index's members (spec end-12a). For a proxy member peryx also unions \
             in what its upstream reports, falling back to the referrers tag schema when the upstream \
             predates the API. A well-formed but unknown subject is an empty list, not a `404`.",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .parameter(query_param(
            "artifactType",
            "Keep only the descriptors whose `artifactType` matches (spec end-12b)",
            json!("application/vnd.dev.cosign.artifact.sig.v1+json"),
        ))
        .response("200", referrers_response("An image index of referrers"))
        .response(
            "400",
            oci_error(
                "`digest` is not a syntactically valid content digest",
                "DIGEST_INVALID",
                "referrers digest is malformed",
            ),
        )
        .response(
            "404",
            oci_error(
                "`name` matches no OCI index route",
                "NAME_UNKNOWN",
                "repository name unknown",
            ),
        )
}

fn oci_referrers_head(reads: ReadExposure) -> OperationBuilder {
    RouteAuth::Read(reads)
        .operation(oci_challenge("The index refuses this read", "repository:<name>:pull"))
        .tag("oci")
        .summary(Some("Check the referrers listing"))
        .description(Some(
            "The referrers listing's headers with no body, which is how a client discovers whether the \
             registry honoured an `artifactType` filter before it pulls the index.",
        ))
        .parameter(name_param())
        .parameter(digest_param())
        .parameter(query_param(
            "artifactType",
            "Keep only the descriptors whose `artifactType` matches (spec end-12b)",
            json!("application/vnd.dev.cosign.artifact.sig.v1+json"),
        ))
        .response("200", referrers_response("The referrers listing's headers"))
        .response(
            "400",
            oci_error(
                "`digest` is not a syntactically valid content digest",
                "DIGEST_INVALID",
                "referrers digest is malformed",
            ),
        )
        .response(
            "404",
            oci_error(
                "`name` matches no OCI index route",
                "NAME_UNKNOWN",
                "repository name unknown",
            ),
        )
}

fn referrers_response(description: &str) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content(
            "application/vnd.oci.image.index.v1+json",
            ContentBuilder::new()
                .example(Some(json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.index.v1+json",
                    "manifests": [{
                        "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "digest": "sha256:9f86...",
                        "size": 779,
                        "artifactType": "application/vnd.dev.cosign.artifact.sig.v1+json",
                    }],
                })))
                .build(),
        )
        .header(
            "OCI-Filters-Applied",
            header("`artifactType`, present only when the request carried that filter"),
        )
}
