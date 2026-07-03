//! The `OpenAPI` description of velodex's HTTP surface.
//!
//! Built programmatically so it lives next to the handlers and is exercised by tests. Served at
//! `/api-docs/openapi.json` and rendered by the documentation site; regenerate the site copy with
//! `velodex openapi > site/static/openapi.json`.

use serde_json::json;
use utoipa::openapi::content::ContentBuilder;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{
    ComponentsBuilder, ContactBuilder, InfoBuilder, LicenseBuilder, OpenApi, OpenApiBuilder, PathsBuilder, Required,
    ResponseBuilder, SecurityRequirement, ServerBuilder,
};

const MIME_SIMPLE_JSON: &str = "application/vnd.pypi.simple.v1+json";

/// The document as pretty JSON, shared by the HTTP endpoint and the `velodex openapi` subcommand.
///
/// # Panics
/// Never in practice: the document is a static structure that always serializes.
#[must_use]
pub fn openapi_json() -> String {
    let mut json = serde_json::to_string_pretty(&openapi()).expect("OpenAPI document always serializes");
    json.push('\n');
    json
}

/// Build the `OpenAPI` 3.1 document for the velodex HTTP API.
#[must_use]
pub fn openapi() -> OpenApi {
    OpenApiBuilder::new()
        .info(
            InfoBuilder::new()
                .title("velodex")
                .version(env!("CARGO_PKG_VERSION"))
                .description(Some(
                    "PyPI-compatible read-through cache and private index. Every configured index route \
                     serves the same surface; `{route}` is the index's route, for example `root/pypi`. \
                     Write operations authenticate with HTTP Basic where the password is the target local \
                     index's upload token and the username is ignored.",
                ))
                .contact(Some(
                    ContactBuilder::new()
                        .name(Some("tox-dev"))
                        .url(Some("https://github.com/tox-dev/velodex"))
                        .build(),
                ))
                .license(Some(LicenseBuilder::new().name("MIT").build()))
                .build(),
        )
        .servers(Some([ServerBuilder::new()
            .url("http://127.0.0.1:4433")
            .description(Some("A local velodex with the default configuration"))
            .build()]))
        .paths(paths())
        .components(Some(
            ComponentsBuilder::new()
                .security_scheme(
                    "uploadToken",
                    SecurityScheme::Http(
                        HttpBuilder::new()
                            .scheme(HttpAuthScheme::Basic)
                            .description(Some(
                                "Any username; the password is the local index's `upload_token` \
                                 (the pypi.org `__token__` convention)",
                            ))
                            .build(),
                    ),
                )
                .build(),
        ))
        .build()
}

fn paths() -> PathsBuilder {
    service_paths(route_paths(PathsBuilder::new()))
}

fn route_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
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
        )
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

fn service_paths(paths: PathsBuilder) -> PathsBuilder {
    paths
        .path(
            "/+status",
            PathItemBuilder::new().operation(HttpMethod::Get, status()).build(),
        )
        .path(
            "/+api",
            PathItemBuilder::new().operation(HttpMethod::Get, discovery()).build(),
        )
        .path(
            "/+search",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, package_search(false))
                .build(),
        )
        .path(
            "/+stats",
            PathItemBuilder::new().operation(HttpMethod::Get, stats()).build(),
        )
        .path(
            "/metrics",
            PathItemBuilder::new().operation(HttpMethod::Get, metrics()).build(),
        )
        .path(
            "/api-docs/openapi.json",
            PathItemBuilder::new()
                .operation(HttpMethod::Get, openapi_endpoint())
                .build(),
        )
}

fn route_param() -> ParameterBuilder {
    ParameterBuilder::new()
        .name("route")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("The index route, for example `root/pypi`"))
        .example(Some(json!("root/pypi")))
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

fn query_param(name: &'static str, description: &'static str, example: serde_json::Value) -> ParameterBuilder {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(ParameterIn::Query)
        .description(Some(description))
        .example(Some(example))
}

fn json_response(description: &str, example: serde_json::Value) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content(MIME_SIMPLE_JSON, ContentBuilder::new().example(Some(example)).build())
}

fn api_json_response(description: &str, example: serde_json::Value) -> ResponseBuilder {
    ResponseBuilder::new()
        .description(description)
        .content("application/json", ContentBuilder::new().example(Some(example)).build())
}

fn text_response(description: &str, content_type: &str, example: &str) -> ResponseBuilder {
    ResponseBuilder::new().description(description).content(
        content_type,
        ContentBuilder::new().example(Some(json!(example))).build(),
    )
}

fn package_search(scoped: bool) -> OperationBuilder {
    let mut operation = OperationBuilder::new()
        .tag("search")
        .summary(Some(if scoped {
            "Search one index route"
        } else {
            "Search cached packages"
        }))
        .description(Some(
            "Searches the derived package index built from cached simple pages, local uploads, \
             and cached core metadata. `q` uses substring matching; prefix it with `re:` for a \
             regex. Results are sorted by display name and paged without collecting every match.",
        ))
        .parameter(query_param(
            "q",
            "Search text. Prefix with `re:` to use a regex.",
            json!("flask"),
        ))
        .parameter(query_param(
            "type",
            "`hosted`, `upstream`, or `upstream-overrides`; omit for all sources.",
            json!("upstream-overrides"),
        ))
        .parameter(query_param("page", "One-based page number.", json!(1)))
        .parameter(query_param("page_size", "Page size: 25, 50, or 100.", json!(25)))
        .response(
            "200",
            api_json_response(
                "Search results",
                json!({
                    "query": "flask",
                    "type": "all",
                    "page": 1,
                    "page_size": 25,
                    "total": 1,
                    "results": [{
                        "display_name": "Flask",
                        "normalized_name": "flask",
                        "route": "root/pypi",
                        "repository": "root/pypi",
                        "type": "upstream",
                        "summary": "A simple framework for building complex web applications.",
                    }],
                }),
            ),
        )
        .response(
            "400",
            api_json_response(
                "Invalid search parameters",
                json!({"error": "invalid package source type"}),
            ),
        );
    if scoped {
        operation = operation.parameter(route_param());
    }
    operation
}

fn index_discovery() -> OperationBuilder {
    OperationBuilder::new()
        .tag("discovery")
        .summary(Some("Discover one index"))
        .description(Some(
            "A compact index document with URLs and client configuration snippets. Snippets appear \
             only when the request has enough host context to render absolute URLs.",
        ))
        .parameter(route_param())
        .response(
            "200",
            api_json_response(
                "The index discovery document",
                json!({
                    "version": "0.0.1",
                    "index": {
                        "name": "root/pypi",
                        "route": "root/pypi",
                        "kind": "overlay",
                        "layers": ["local", "pypi"],
                        "uploads": true,
                        "upload_to": "local",
                        "capabilities": {
                            "simple_html": true,
                            "simple_json": true,
                            "simple_api_version": velodex_core::pypi::API_VERSION,
                            "metadata_siblings": true,
                            "uploads": true,
                            "yanking": true,
                            "volatile_deletes": true,
                            "project_status": true,
                            "provenance": true,
                            "legacy_json": false
                        },
                        "urls": {
                            "api": "http://127.0.0.1:4433/root/pypi/+api",
                            "simple": "http://127.0.0.1:4433/root/pypi/simple/",
                            "upload": "http://127.0.0.1:4433/root/pypi/",
                            "status": "http://127.0.0.1:4433/+status",
                            "web": "http://127.0.0.1:4433/browse?index=root%2Fpypi",
                            "stats": "http://127.0.0.1:4433/stats?index=root%2Fpypi",
                            "openapi": "http://127.0.0.1:4433/api-docs/openapi.json"
                        },
                        "client_configuration": {
                            "pip.conf": "[global]\nindex-url = http://127.0.0.1:4433/root/pypi/simple/\n",
                            "uv.toml": "publish-url = \"http://127.0.0.1:4433/root/pypi/\"\n\n[[index]]\nname = \"velodex\"\nurl = \"http://127.0.0.1:4433/root/pypi/simple/\"\ndefault = true\n\n[pip]\nindex-url = \"http://127.0.0.1:4433/root/pypi/simple/\"\n",
                            ".pypirc": "[distutils]\nindex-servers =\n    velodex\n\n[velodex]\nrepository = http://127.0.0.1:4433/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
                        }
                    }
                }),
            ),
        )
        .response("404", ResponseBuilder::new().description("No index at this route"))
}

fn discovery() -> OperationBuilder {
    OperationBuilder::new()
        .tag("discovery")
        .summary(Some("Discover this server"))
        .description(Some(
            "A compact server document with global URLs and one discovery entry per configured \
             index. It is built from configuration and request context, without reading package \
             indexes.",
        ))
        .response(
            "200",
            api_json_response(
                "The server discovery document",
                json!({
                    "version": "0.0.1",
                    "urls": {
                        "api": "http://127.0.0.1:4433/+api",
                        "status": "http://127.0.0.1:4433/+status",
                        "stats": "http://127.0.0.1:4433/+stats",
                        "openapi": "http://127.0.0.1:4433/api-docs/openapi.json",
                        "web": "http://127.0.0.1:4433/"
                    },
                    "indexes": []
                }),
            ),
        )
}

fn project_list() -> OperationBuilder {
    OperationBuilder::new()
        .tag("simple")
        .summary(Some("List projects"))
        .description(Some(
            "The projects velodex has observed on this index: everything uploaded, plus every mirrored \
             project a client has asked for. An overlay unions its layers. JSON or HTML by `Accept`.",
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
            "All files of one project, merged across overlay layers (first match per filename wins, \
             versions union). File URLs point back at velodex's own `files/` route; `core-metadata` \
             advertises the PEP 658 sibling.",
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
        .response("404", ResponseBuilder::new().description("No layer of this index has the project"))
        .response("502", ResponseBuilder::new().description("The upstream failed and nothing is cached"))
}

fn file_download() -> OperationBuilder {
    OperationBuilder::new()
        .tag("files")
        .summary(Some("Download an artifact"))
        .description(Some(
            "Serves the blob if cached; otherwise fetches it from its source mirror, verifies the \
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
            ResponseBuilder::new().description("Project status does not allow downloads"),
        )
        .response(
            "502",
            ResponseBuilder::new().description("The source mirror failed or the bytes did not match the digest"),
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
            ResponseBuilder::new().description("Project status does not allow downloads"),
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
            ResponseBuilder::new().description(
                "Uploads disabled: the local index has no `upload_token`, or project status rejects uploads",
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
         succeed. Uploaded files get their record updated; files served from a read-only mirror get \
         a reversible override on the overlay's local layer, so upstream releases can be yanked too. \
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
        "Clears the hidden marker a DELETE leaves on files served from a read-only mirror, making \
         them visible on the overlay again. Omit `{version}/` to restore the whole project.",
        "affected 1 file(s)",
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
         for an overlay, the upstream files become visible again.",
        "removed 1 file(s)",
    )
    .response(
        "403",
        ResponseBuilder::new().description("The index is not volatile; delete is disabled"),
    )
}

fn delete_project() -> OperationBuilder {
    let operation = removal_operation(
        "Delete a project",
        "Removes every uploaded file of the project. Requires the local layer to be `volatile`.",
        "removed 3 file(s)",
    )
    .response(
        "403",
        ResponseBuilder::new().description("The index is not volatile; delete is disabled"),
    );
    // The project-level path has no {version} parameter.
    operation
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

fn status() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Health and identity"))
        .description(Some(
            "Version, counters, and the configured indexes. Add `?details=admin` for bounded metadata \
             summaries used by the read-only admin status page.",
        ))
        .parameter(
            ParameterBuilder::new()
                .name("details")
                .parameter_in(ParameterIn::Query)
                .description(Some(
                    "Use `admin` to include observed project counts, uploaded file counts, and recent uploads.",
                ))
                .example(Some(json!("admin"))),
        )
        .response(
            "200",
            ResponseBuilder::new().description("The status document").content(
                "application/json",
                ContentBuilder::new()
                    .example(Some(json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "serial": 42,
                        "requests": 128,
                        "metadata_requests": 37,
                        "indexes": [
                            {"name": "pypi", "route": "pypi", "kind": "mirror", "layers": [],
                             "uploads": false, "volatile_deletes": false, "upload_to": null,
                             "upstream": {"url": "https://pypi.org/simple/", "auth": {"kind": "none", "redacted": null}, "status": "configured"},
                             "local": null, "project_count": 128, "upload_count": 0, "recent_uploads": []},
                            {"name": "local", "route": "local", "kind": "local", "layers": [],
                             "uploads": true, "volatile_deletes": true, "upload_to": null, "upstream": null,
                             "local": {"volatile": true, "upload_token": {"configured": true, "redacted": "<redacted>"}},
                             "project_count": 2, "upload_count": 4,
                             "recent_uploads": [{"project": "velodexpkg", "filename": "velodexpkg-1.0-py3-none-any.whl",
                                                "version": "1.0", "uploaded_at": "2026-01-01T00:00:00Z", "size": 1832}]},
                            {"name": "root/pypi", "route": "root/pypi", "kind": "overlay",
                             "layers": ["local", "pypi"], "uploads": true, "volatile_deletes": true,
                             "upload_to": "local",
                             "upstream": null, "local": null, "project_count": 0, "upload_count": 0,
                             "recent_uploads": []}
                        ]
                    })))
                    .build(),
            ),
        )
}

fn stats() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Usage statistics"))
        .description(Some(
            "Counters aggregated off the request path, drillable: no parameters for per-index totals, \
             `?index={route}` for one index's projects, `&project={name}` for one project's files. \
             Counters cover pages, downloads (with bytes), metadata, uploads, refreshes, upstream \
             changes, stale fallbacks, upstream errors, and rejected downloads.",
        ))
        .parameter(
            ParameterBuilder::new()
                .name("index")
                .parameter_in(ParameterIn::Query)
                .description(Some("Drill into one index's projects"))
                .example(Some(json!("root/pypi"))),
        )
        .parameter(
            ParameterBuilder::new()
                .name("project")
                .parameter_in(ParameterIn::Query)
                .description(Some("With `index`, drill into one project's files"))
                .example(Some(json!("pandas"))),
        )
        .response(
            "200",
            ResponseBuilder::new()
                .description("The counters at the requested depth")
                .content(
                    "application/json",
                    ContentBuilder::new()
                        .example(Some(json!({
                            "root/pypi": {
                                "pages": 12, "downloads": 6, "metadata": 6, "uploads": 0,
                                "bytes": 64_733_247, "refreshes": 2, "changed": 1,
                                "stale_served": 0, "upstream_errors": 0, "rejected": 0
                            }
                        })))
                        .build(),
                ),
        )
}

fn metrics() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("Prometheus metrics"))
        .response(
            "200",
            text_response(
                "Prometheus text exposition",
                "text/plain; version=0.0.4",
                "# HELP velodex_requests_total Total HTTP requests served.\n\
                 # TYPE velodex_requests_total counter\n\
                 velodex_requests_total 128\n\
                 # HELP velodex_metadata_requests_total PEP 658 .metadata siblings served.\n\
                 # TYPE velodex_metadata_requests_total counter\n\
                 velodex_metadata_requests_total 37\n",
            ),
        )
}

fn openapi_endpoint() -> OperationBuilder {
    OperationBuilder::new()
        .tag("operations")
        .summary(Some("This document"))
        .response(
            "200",
            ResponseBuilder::new()
                .description("The OpenAPI 3.1 description of this server")
                .content("application/json", ContentBuilder::new().build()),
        )
}
