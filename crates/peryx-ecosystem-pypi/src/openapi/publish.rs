//! Publishing and mutating a hosted index: upload, yank, restore, promote, delete.

#[allow(clippy::wildcard_imports, reason = "shared is this module's OpenAPI-builder prelude")]
use super::shared::*;

pub(super) fn upload() -> OperationBuilder {
    OperationBuilder::new()
        .tag("publish")
        .summary(Some("Upload a distribution"))
        .description(Some(
            "The legacy PyPI upload API, as sent by `twine upload` and `uv publish`. The multipart \
             form's `content` part carries a wheel or a `.tar.gz` or `.zip` sdist. peryx streams the \
             bytes to a staged blob, verifies declared `sha256_digest` and `blake2_256_digest` \
             values, then checks filename, archive, wheel `.dist-info` structure, RECORD hashes, and \
             sdist top-level structure before the file lands in the index's local layer. Uploaded \
             wheels and sdists get PEP 658/714 `.metadata` siblings from verified core metadata. The \
             upload shadows any upstream file of the same name.",
        ))
        .parameter(route_param())
        .security(SecurityRequirement::new("uploadToken", [""; 0]))
        .request_body(Some(
            RequestBodyBuilder::new()
                .description(Some("`multipart/form-data` with `:action=file_upload`"))
                .content(
                    "multipart/form-data",
                    ContentBuilder::new()
                        .example(Some(json!({
                            ":action": "file_upload",
                            "name": "peryxpkg",
                            "version": "1.0",
                            "filetype": "bdist_wheel",
                            "requires_python": ">=3.8",
                            "sha256_digest": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
                            "blake2_256_digest": "5c7d8a29df3e0c4c752c02bff90615dd08d1d1aa9e23738a1e50c27f0415f95d",
                            "content": "<the distribution file>"
                        })))
                        .build(),
                )
                .build(),
        ))
        .response("200", text_response("Stored", "text/plain", "upload accepted"))
        .response(
            "400",
            text_response(
                "Rejected",
                "text/plain",
                "metadata Name \"other\" does not match upload name \"peryxpkg\"",
            ),
        )
        .response("401", ResponseBuilder::new().description("Missing or wrong token"))
        .response(
            "403",
            policy_denial_response(
                "Uploads disabled, project status rejects uploads, or index policy denied the upload",
                "upload",
            ),
        )
        .response(
            "405",
            ResponseBuilder::new().description("The route's index has no writable local layer"),
        )
}
pub(super) fn yank() -> OperationBuilder {
    removal_operation(
        "Yank files",
        "Marks the version's files yanked (PEP 592): resolvers skip them, exact-pin installs still \
         succeed. Uploaded files get their record updated; files served from a read-only cache get \
         a reversible override on the virtual index's hosted layer, so upstream releases can be yanked too. \
         Omit `{version}/` (i.e. `PUT /{route}/{project}/yank`) to yank the whole project. Add \
         `?reason=...` to preserve a resolver-visible reason.",
        "affected 1 file(s)",
    )
    .parameter(
        ParameterBuilder::new()
            .name("reason")
            .parameter_in(ParameterIn::Query)
            .description(Some("Optional PEP 592 yank reason preserved in Simple API output"))
            .example(Some(json!("build metadata is incorrect"))),
    )
}
pub(super) fn restore() -> OperationBuilder {
    removal_operation(
        "Restore hidden files",
        "Clears the hidden marker a DELETE leaves on files served from a read-only cache, making \
         them visible on the virtual index again. Omit `{version}/` to restore the whole project.",
        "affected 1 file(s)",
    )
}
pub(super) fn promote() -> OperationBuilder {
    OperationBuilder::new()
        .tag("publish")
        .summary(Some("Promote a release"))
        .description(Some(
            "Copies uploaded file records for one release from the source route's local upload \
             layer into this route's local upload layer. Promotion reuses content-addressed blob \
             digests and preserves sha256, size, upload time, yank state, and metadata sibling \
             hashes. The target route's upload token authenticates the request. Archived and \
             quarantined target projects reject promotion through the same project-status policy \
             used for uploads.",
        ))
        .parameter(route_param())
        .parameter(project_param())
        .parameter(version_param())
        .parameter(
            ParameterBuilder::new()
                .name("from")
                .parameter_in(ParameterIn::Query)
                .required(Required::True)
                .description(Some("Source route whose local upload layer contains the release"))
                .example(Some(json!("staging"))),
        )
        .security(SecurityRequirement::new("uploadToken", [""; 0]))
        .response(
            "200",
            text_response(
                "Done; the body counts promoted files",
                "text/plain",
                "promoted 2 file(s)",
            ),
        )
        .response(
            "400",
            ResponseBuilder::new().description("Missing `from`, unsafe path segment, or missing version"),
        )
        .response("401", ResponseBuilder::new().description("Missing or wrong token"))
        .response(
            "403",
            ResponseBuilder::new().description("Project status rejects writes on the target route"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("Unknown route or no source release matched"),
        )
        .response(
            "409",
            ResponseBuilder::new().description("A target filename already exists with a different sha256"),
        )
        .response(
            "405",
            ResponseBuilder::new().description("The source or target route has no writable local layer"),
        )
}
pub(super) fn unyank() -> OperationBuilder {
    removal_operation(
        "Un-yank files",
        "Clears the PEP 592 yank marker set by the PUT form of this path.",
        "removed 1 file(s)",
    )
}
pub(super) fn delete_version() -> OperationBuilder {
    removal_operation(
        "Delete a version",
        "Removes the version's uploaded files outright. Requires the local layer to be `volatile`; \
         for a virtual index, the upstream files become visible again.",
        "removed 1 file(s)",
    )
    .response(
        "403",
        ResponseBuilder::new().description("The index is not volatile; delete is disabled"),
    )
}
pub(super) fn delete_project() -> OperationBuilder {
    removal_operation(
        "Delete a project",
        "Removes every uploaded file of the project. Requires the local layer to be `volatile`.",
        "removed 3 file(s)",
    )
    .response(
        "403",
        ResponseBuilder::new().description("The index is not volatile; delete is disabled"),
    )
}
pub(super) fn removal_operation(summary: &str, description: &str, example: &str) -> OperationBuilder {
    OperationBuilder::new()
        .tag("publish")
        .summary(Some(summary))
        .description(Some(description))
        .parameter(route_param())
        .parameter(project_param())
        .parameter(version_param())
        .security(SecurityRequirement::new("uploadToken", [""; 0]))
        .response(
            "200",
            text_response("Done; the body counts affected files", "text/plain", example),
        )
        .response("401", ResponseBuilder::new().description("Missing or wrong token"))
        .response("404", ResponseBuilder::new().description("Nothing matched"))
        .response(
            "405",
            ResponseBuilder::new().description("The route's index has no writable local layer"),
        )
}
