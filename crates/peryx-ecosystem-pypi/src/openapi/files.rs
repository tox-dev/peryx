//! Downloading an artifact and its PEP 658 metadata sibling.

#[allow(clippy::wildcard_imports, reason = "shared is this module's OpenAPI-builder prelude")]
use super::shared::*;

pub(super) fn file_download() -> OperationBuilder {
    OperationBuilder::new()
        .tag("files")
        .summary(Some("Download an artifact"))
        .description(Some(
            "Serves the blob if cached; otherwise fetches it from its upstream cache, verifies the \
             sha256, caches it, and serves it. The `{filename}` segment is display context and must \
             be percent-encoded as one path segment. Responses are immutable \
             (`Cache-Control: max-age=31536000`) and carry the quoted sha256 as their `ETag`. A cached \
             blob honors a single byte range, so an interrupted download can resume; a range this \
             server cannot read is ignored and the whole artifact served.",
        ))
        .parameter(route_param())
        .parameter(sha256_param())
        .parameter(filename_param("peryxpkg-1.0-py3-none-any.whl"))
        .parameter(range_param())
        .parameter(if_none_match_param())
        .response(
            "200",
            ResponseBuilder::new()
                .description("The artifact bytes")
                .content("application/octet-stream", ContentBuilder::new().build()),
        )
        .response(
            "304",
            ResponseBuilder::new().description("`If-None-Match` names the artifact's digest, so no body is sent"),
        )
        .response(
            "206",
            ResponseBuilder::new()
                .description("The requested byte range of a cached artifact")
                .content("application/octet-stream", ContentBuilder::new().build()),
        )
        .response(
            "416",
            ResponseBuilder::new().description("The range lies outside the artifact; `Content-Range` names its size"),
        )
        .response(
            "400",
            ResponseBuilder::new()
                .description("The digest is not 64 lowercase hex, or the filename is not a safe path segment"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("No file with this digest is known"),
        )
        .response(
            "403",
            policy_denial_response("Project status or index policy does not allow downloads", "serve"),
        )
        .response(
            "502",
            ResponseBuilder::new().description("The upstream cache failed or the bytes did not match the digest"),
        )
}
pub(super) fn metadata_download() -> OperationBuilder {
    OperationBuilder::new()
        .tag("files")
        .summary(Some("Download PEP 658 core metadata"))
        .description(Some(
            "The `.metadata` sibling of an artifact: advertised upstream metadata verified against \
             the index-page digest, or metadata generated from a wheel `METADATA`/sdist `PKG-INFO` \
             when upstream omits the sibling. pip and uv resolve through this instead of downloading \
             whole artifacts.",
        ))
        .parameter(route_param())
        .parameter(sha256_param())
        .parameter(filename_param("peryxpkg-1.0-py3-none-any.whl.metadata"))
        .response(
            "200",
            text_response(
                "The core-metadata document",
                "application/octet-stream",
                "Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\nRequires-Python: >=3.8\n",
            ),
        )
        .response(
            "404",
            ResponseBuilder::new().description("The artifact has no known metadata sibling"),
        )
        .response(
            "403",
            policy_denial_response("Project status or index policy does not allow downloads", "serve"),
        )
}
