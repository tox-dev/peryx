//! Browsing inside a stored archive.

#[allow(clippy::wildcard_imports, reason = "shared is this module's OpenAPI-builder prelude")]
use super::shared::*;

pub(super) fn inspect_listing() -> OperationBuilder {
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
        .parameter(filename_param("peryxpkg-1.0-py3-none-any.whl"))
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
                .example(Some(json!("peryxpkg-1.0.dist-info/METADATA"))),
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
                            "filename": "peryxpkg-1.0-py3-none-any.whl",
                            "members": [
                                {"path": "peryxpkg/__init__.py", "size": 20, "kind": "text", "previewable": true},
                                {"path": "vendor/inner.zip", "size": 1102, "kind": "archive", "previewable": false},
                                {"path": "peryxpkg/data.bin", "size": 8192, "kind": "binary", "previewable": false}
                            ]
                        })))
                        .build(),
                )
                .content(
                    "text/plain; charset=utf-8",
                    ContentBuilder::new()
                        .example(Some(json!("Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n")))
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
pub(super) fn inspect_member() -> OperationBuilder {
    OperationBuilder::new()
        .tag("files")
        .summary(Some("Read an archive member"))
        .description(Some(
            "Legacy path form for reading one root archive member as bounded text chunks. Prefer \
             the query form when member names contain slashes or when inspecting nested archives.",
        ))
        .parameter(route_param())
        .parameter(sha256_param())
        .parameter(filename_param("peryxpkg-1.0-py3-none-any.whl"))
        .parameter(
            ParameterBuilder::new()
                .name("member")
                .parameter_in(ParameterIn::Path)
                .required(Required::True)
                .example(Some(json!("peryxpkg-1.0.dist-info/METADATA"))),
        )
        .response(
            "200",
            text_response(
                "The member content",
                "text/plain; charset=utf-8",
                "Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n",
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
