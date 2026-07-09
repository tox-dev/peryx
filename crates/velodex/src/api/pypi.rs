//! The `PyPI` Simple API, legacy JSON, file, inspect, and publish operations.

use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::{PathsBuilder, Required, ResponseBuilder, SecurityRequirement};

use super::service::{index_discovery, package_search};
use super::shared::{MIME_SIMPLE_JSON, api_json_response, route_param, text_response};

pub(super) fn route_paths(paths: PathsBuilder) -> PathsBuilder {
    let paths = paths
        .path(
            "/{route}/simple/",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, project_list())
                .build(),
        )
        .path(
            "/{route}/simple/{project}/",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, project_detail())
                .build(),
        );
    let paths = legacy_json_paths(paths);
    paths
        .path(
            "/{route}/files/{sha256}/{filename}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, file_download())
                .build(),
        )
        .path(
            "/{route}/files/{sha256}/{filename}.metadata",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, metadata_download())
                .build(),
        )
        .path(
            "/{route}/",
            PathItemBuilder::new().operation(HttpMethod::Post, upload()).build(),
        )
        .path(
            "/{route}/+api",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, index_discovery())
                .build(),
        )
        .path(
            "/{route}/+search",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, package_search(true))
                .build(),
        )
        .path(
            "/{route}/inspect/{sha256}/{filename}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, inspect_listing())
                .build(),
        )
        .path(
            "/{route}/inspect/{sha256}/{filename}/{member}",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, inspect_member())
                .build(),
        )
        .path(
            "/{route}/{project}/{version}/yank",
            PathItemBuilder::new()
                .operation(HttpMethod::Put, yank())
                .operation(HttpMethod::Delete, unyank())
                .build(),
        )
        .path(
            "/{route}/{project}/{version}/restore",
            PathItemBuilder::new().operation(HttpMethod::Put, restore()).build(),
        )
        .path(
            "/{route}/{project}/{version}/promote",
            PathItemBuilder::new().operation(HttpMethod::Put, promote()).build(),
        )
        .path(
            "/{route}/{project}/{version}/",
            PathItemBuilder::new()
                .operation(HttpMethod::Delete, delete_version())
                .build(),
        )
        .path(
            "/{route}/{project}/",
            PathItemBuilder::new()
                .operation(HttpMethod::Delete, delete_project())
                .build(),
        )
}

fn legacy_json_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/{route}/{project}/json",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, legacy_project_json())
                .build(),
        )
        .path(
            "/{route}/{project}/{version}/json",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, legacy_release_json())
                .build(),
        )
}

fn project_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("project")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The normalized (PEP 503) project name"))
        .example(Some(json!("requests")))
}

fn version_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("version")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("One release version"))
        .example(Some(json!("1.2.0")))
}

fn accept_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("Accept")
        .parameter_in(ParameterIn::Header)
        .description(Some(
            "`application/vnd.pypi.simple.v1+json` for PEP 691 JSON; anything else gets PEP 503 HTML",
        ))
        .example(Some(json!(MIME_SIMPLE_JSON)))
}

fn json_response(description: &str, example: serde_json::Value) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content(MIME_SIMPLE_JSON, ContentBuilder::new().example(Some(example)).build())
}

fn policy_denial_response(description: &str, action: &str) -> ResponseBuilder {
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

fn project_list() -> OperationBuilder {
    OperationBuilder::new()
        .tag("simple")
        .summary(Some("List projects"))
        .description(Some(
            "The projects velodex has observed on this index: everything uploaded, plus every mirrored \
             project a client has asked for. A virtual index unions its layers. Index policy filters \
             denied projects before serialization. JSON or HTML by `Accept`.",
        ))
        .parameter(route_param())
        .parameter(accept_param())
        .response(
            "200",
            json_response(
                "The project list (PEP 691 shown; PEP 503 HTML without the JSON `Accept`)",
                json!({
                    "meta": {"api-version": "1.4"},
                    "projects": [{"name": "requests"}, {"name": "velodexpkg"}]
                }),
            ),
        )
        .response("404", ResponseBuilder::new().description("No index at this route"))
}

fn project_detail() -> OperationBuilder {
    OperationBuilder::new()
        .tag("simple")
        .summary(Some("Project detail"))
        .description(Some(
            "All files of one project, merged across virtual-index layers (first match per filename wins, \
             versions union). File URLs point back at velodex's own `files/` route; `core-metadata` \
             advertises the PEP 658 sibling. Index policy filters denied files and their \
             versions before serialization.",
        ))
        .parameter(route_param())
        .parameter(project_param())
        .parameter(accept_param())
        .response(
            "200",
            json_response(
                "The project detail page",
                json!({
                    "meta": {"api-version": "1.4", "project-status": "active"},
                    "name": "velodexpkg",
                    "versions": ["1.0"],
                    "files": [{
                        "filename": "velodexpkg-1.0-py3-none-any.whl",
                        "url": "/root/pypi/files/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08/velodexpkg-1.0-py3-none-any.whl",
                        "hashes": {"sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"},
                        "requires-python": ">=3.8",
                        "size": 1832,
                        "upload-time": "2026-01-01T00:00:00Z",
                        "yanked": false,
                        "core-metadata": {"sha256": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"},
                        "dist-info-metadata": {"sha256": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"},
                        "gpg-sig": false,
                        "provenance": "https://example.test/provenance/velodexpkg-1.0-py3-none-any.whl"
                    }]
                }),
            ),
        )
        .response(
            "403",
            policy_denial_response("Index policy denied the project detail", "serve"),
        )
        .response("404", ResponseBuilder::new().description("No layer of this index has the project"))
        .response("502", ResponseBuilder::new().description("The upstream failed and nothing is cached"))
}

fn legacy_project_json() -> OperationBuilder {
    OperationBuilder::new()
        .tag("legacy")
        .summary(Some("Legacy project JSON"))
        .description(Some(
            "PyPI's legacy project JSON shape, built from the resolved Simple API detail page. The \
             `releases` and `urls` file entries preserve hashes, yanked markers, upload time, \
             size, and `requires_python`; metadata that Simple pages do not expose is null, empty, \
             or `-1`.",
        ))
        .parameter(route_param())
        .parameter(project_param())
        .response(
            "200",
            api_json_response(
                "The legacy project JSON document",
                json!({
                    "info": {
                        "name": "velodexpkg",
                        "version": "1.0",
                        "requires_python": ">=3.8",
                        "summary": "",
                        "downloads": {"last_day": -1, "last_month": -1, "last_week": -1},
                        "yanked": false,
                        "yanked_reason": null
                    },
                    "last_serial": 0,
                    "releases": {
                        "1.0": [{
                            "comment_text": "",
                            "digests": {"sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"},
                            "downloads": -1,
                            "filename": "velodexpkg-1.0-py3-none-any.whl",
                            "has_sig": false,
                            "md5_digest": null,
                            "packagetype": "bdist_wheel",
                            "python_version": "py3",
                            "requires_python": ">=3.8",
                            "size": 1832,
                            "upload_time": "2026-01-01T00:00:00",
                            "upload_time_iso_8601": "2026-01-01T00:00:00Z",
                            "url": "/root/pypi/files/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08/velodexpkg-1.0-py3-none-any.whl",
                            "yanked": false,
                            "yanked_reason": null
                        }]
                    },
                    "urls": [{
                        "comment_text": "",
                        "digests": {"sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"},
                        "downloads": -1,
                        "filename": "velodexpkg-1.0-py3-none-any.whl",
                        "has_sig": false,
                        "md5_digest": null,
                        "packagetype": "bdist_wheel",
                        "python_version": "py3",
                        "requires_python": ">=3.8",
                        "size": 1832,
                        "upload_time": "2026-01-01T00:00:00",
                        "upload_time_iso_8601": "2026-01-01T00:00:00Z",
                        "url": "/root/pypi/files/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08/velodexpkg-1.0-py3-none-any.whl",
                        "yanked": false,
                        "yanked_reason": null
                    }],
                    "vulnerabilities": [],
                    "ownership": {"roles": [], "organization": null}
                }),
            ),
        )
        .response("404", ResponseBuilder::new().description("No layer has the project"))
        .response("502", ResponseBuilder::new().description("The upstream failed and nothing is cached"))
}

fn legacy_release_json() -> OperationBuilder {
    OperationBuilder::new()
        .tag("legacy")
        .summary(Some("Legacy release JSON"))
        .description(Some(
            "Version-specific legacy JSON. This has the same `info`, `urls`, `vulnerabilities`, \
             and `ownership` shape as the project endpoint, without the deprecated `releases` map.",
        ))
        .parameter(route_param())
        .parameter(project_param())
        .parameter(version_param())
        .response(
            "200",
            api_json_response(
                "The legacy release JSON document",
                json!({
                    "info": {
                        "name": "velodexpkg",
                        "version": "1.0",
                        "requires_python": ">=3.8",
                        "summary": "",
                        "downloads": {"last_day": -1, "last_month": -1, "last_week": -1},
                        "yanked": false,
                        "yanked_reason": null
                    },
                    "last_serial": 0,
                    "urls": [{
                        "comment_text": "",
                        "digests": {"sha256": "0ace7980f82c5815ede4cd7bf9f6693684cec2ae47b9b7ade9add533b8627c6b"},
                        "downloads": -1,
                        "filename": "velodexpkg-1.0.tar.gz",
                        "has_sig": false,
                        "md5_digest": null,
                        "packagetype": "sdist",
                        "python_version": "source",
                        "requires_python": ">=3.8",
                        "size": 5760,
                        "upload_time": "2026-01-01T00:00:01",
                        "upload_time_iso_8601": "2026-01-01T00:00:01Z",
                        "url": "/root/pypi/files/0ace7980f82c5815ede4cd7bf9f6693684cec2ae47b9b7ade9add533b8627c6b/velodexpkg-1.0.tar.gz",
                        "yanked": false,
                        "yanked_reason": null
                    }],
                    "vulnerabilities": [],
                    "ownership": {"roles": [], "organization": null}
                }),
            ),
        )
        .response("404", ResponseBuilder::new().description("No layer has the project or version"))
        .response("502", ResponseBuilder::new().description("The upstream failed and nothing is cached"))
}

fn file_download() -> OperationBuilder {
    OperationBuilder::new()
        .tag("files")
        .summary(Some("Download an artifact"))
        .description(Some(
            "Serves the blob if cached; otherwise fetches it from its upstream cache, verifies the \
             sha256, caches it, and serves it. The `{filename}` segment is display context and must \
             be percent-encoded as one path segment. Responses are immutable \
             (`Cache-Control: max-age=31536000`).",
        ))
        .parameter(route_param())
        .parameter(sha256_param())
        .parameter(filename_param("velodexpkg-1.0-py3-none-any.whl"))
        .response(
            "200",
            ResponseBuilder::new()
                .description("The artifact bytes")
                .content("application/octet-stream", ContentBuilder::new().build()),
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

fn metadata_download() -> OperationBuilder {
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
        .parameter(filename_param("velodexpkg-1.0-py3-none-any.whl.metadata"))
        .response(
            "200",
            text_response(
                "The core-metadata document",
                "application/octet-stream",
                "Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\nRequires-Python: >=3.8\n",
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

fn sha256_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("sha256")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The artifact's sha256, lowercase hex"))
        .example(Some(json!(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        )))
}

fn filename_param(example: &str) -> ParameterBuilder {
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

fn upload() -> OperationBuilder {
    OperationBuilder::new()
        .tag("publish")
        .summary(Some("Upload a distribution"))
        .description(Some(
            "The legacy PyPI upload API, as sent by `twine upload` and `uv publish`. The multipart \
             form's `content` part carries a wheel or modern `.tar.gz` sdist. velodex streams the \
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
                            "name": "velodexpkg",
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
                "metadata Name \"other\" does not match upload name \"velodexpkg\"",
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

fn yank() -> OperationBuilder {
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

fn restore() -> OperationBuilder {
    removal_operation(
        "Restore hidden files",
        "Clears the hidden marker a DELETE leaves on files served from a read-only cache, making \
         them visible on the virtual index again. Omit `{version}/` to restore the whole project.",
        "affected 1 file(s)",
    )
}

fn promote() -> OperationBuilder {
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

fn inspect_listing() -> OperationBuilder {
    OperationBuilder::new()
        .tag("files")
        .summary(Some("List archive members"))
        .description(Some(
            "The file members of a cached wheel, zip, zipped egg, plain tarball, or gzip tarball \
             (`.tar.gz` and `.tgz`). Nested archives are not expanded unless the request names \
             them with repeated `container` query parameters. Pass `member` to read one bounded \
             text chunk.",
        ))
        .parameter(route_param())
        .parameter(sha256_param())
        .parameter(filename_param("velodexpkg-1.0-py3-none-any.whl"))
        .parameter(
            ParameterBuilder::new()
                .name("container")
                .parameter_in(ParameterIn::Query)
                .description(Some(
                    "Optional archive-member path to treat as the next archive level. Repeat in stack order.",
                ))
                .example(Some(json!("vendor/inner.zip"))),
        )
        .parameter(
            ParameterBuilder::new()
                .name("member")
                .parameter_in(ParameterIn::Query)
                .description(Some("Optional text member path to read as a bounded chunk"))
                .example(Some(json!("velodexpkg-1.0.dist-info/METADATA"))),
        )
        .parameter(
            ParameterBuilder::new()
                .name("offset")
                .parameter_in(ParameterIn::Query)
                .description(Some("Byte offset inside the selected member; defaults to 0"))
                .example(Some(json!(262_144))),
        )
        .parameter(
            ParameterBuilder::new()
                .name("limit")
                .parameter_in(ParameterIn::Query)
                .description(Some(
                    "Maximum bytes to return, from 1 through 1048576; defaults to 262144",
                ))
                .example(Some(json!(262_144))),
        )
        .response(
            "200",
            ResponseBuilder::new()
                .description("The archive listing, or a text member chunk when `member` is set")
                .content(
                    "application/json",
                    ContentBuilder::new()
                        .example(Some(json!({
                            "filename": "velodexpkg-1.0-py3-none-any.whl",
                            "members": [
                                {"path": "velodexpkg/__init__.py", "size": 20, "kind": "text", "previewable": true},
                                {"path": "vendor/inner.zip", "size": 1102, "kind": "archive", "previewable": false},
                                {"path": "velodexpkg/data.bin", "size": 8192, "kind": "binary", "previewable": false}
                            ]
                        })))
                        .build(),
                )
                .content(
                    "text/plain; charset=utf-8",
                    ContentBuilder::new()
                        .example(Some(json!("Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n")))
                        .build(),
                ),
        )
        .response(
            "413",
            ResponseBuilder::new().description("Nested archive size or listed entries exceed limits"),
        )
        .response(
            "400",
            ResponseBuilder::new().description("Bad digest, unsafe filename, or archive nesting depth above the limit"),
        )
        .response(
            "404",
            ResponseBuilder::new().description("No file with this digest is known, or the member does not exist"),
        )
        .response(
            "415",
            ResponseBuilder::new().description("Not a supported archive type, or the selected member is not text"),
        )
        .response(
            "416",
            ResponseBuilder::new().description("The requested member offset is beyond the member size"),
        )
        .response("422", ResponseBuilder::new().description("The archive cannot be read"))
}

fn inspect_member() -> OperationBuilder {
    OperationBuilder::new()
        .tag("files")
        .summary(Some("Read an archive member"))
        .description(Some(
            "Legacy path form for reading one root archive member as bounded text chunks. Prefer \
             the query form when member names contain slashes or when inspecting nested archives.",
        ))
        .parameter(route_param())
        .parameter(sha256_param())
        .parameter(filename_param("velodexpkg-1.0-py3-none-any.whl"))
        .parameter(
            ParameterBuilder::new()
                .name("member")
                .parameter_in(ParameterIn::Path)
                .required(Required::True)
                .example(Some(json!("velodexpkg-1.0.dist-info/METADATA"))),
        )
        .response(
            "200",
            text_response(
                "The member content",
                "text/plain; charset=utf-8",
                "Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n",
            ),
        )
        .response("404", ResponseBuilder::new().description("Unknown digest or member"))
        .response(
            "415",
            ResponseBuilder::new().description("The member is not previewable text"),
        )
        .response(
            "416",
            ResponseBuilder::new().description("The requested offset is beyond the member size"),
        )
}

fn unyank() -> OperationBuilder {
    removal_operation(
        "Un-yank files",
        "Clears the PEP 592 yank marker set by the PUT form of this path.",
        "removed 1 file(s)",
    )
}

fn delete_version() -> OperationBuilder {
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

fn delete_project() -> OperationBuilder {
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

fn removal_operation(summary: &str, description: &str, example: &str) -> OperationBuilder {
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
