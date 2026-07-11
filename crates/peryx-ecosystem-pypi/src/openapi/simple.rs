//! The Simple repository API: the project list and a project's detail page.

#[allow(clippy::wildcard_imports, reason = "shared is this module's OpenAPI-builder prelude")]
use super::shared::*;

pub(super) fn project_list() -> OperationBuilder {
    OperationBuilder::new()
        .tag("simple")
        .summary(Some("List projects"))
        .description(Some(
            "The projects peryx has observed on this index: everything uploaded, plus every mirrored \
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
                    "projects": [{"name": "requests"}, {"name": "peryxpkg"}]
                }),
            ),
        )
        .response("404", ResponseBuilder::new().description("No index at this route"))
}
pub(super) fn project_detail() -> OperationBuilder {
    OperationBuilder::new()
        .tag("simple")
        .summary(Some("Project detail"))
        .description(Some(
            "All files of one project, merged across virtual-index layers (first match per filename wins, \
             versions union). File URLs point back at peryx's own `files/` route; `core-metadata` \
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
                    "name": "peryxpkg",
                    "versions": ["1.0"],
                    "files": [{
                        "filename": "peryxpkg-1.0-py3-none-any.whl",
                        "url": "/root/pypi/files/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08/peryxpkg-1.0-py3-none-any.whl",
                        "hashes": {"sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"},
                        "requires-python": ">=3.8",
                        "size": 1832,
                        "upload-time": "2026-01-01T00:00:00Z",
                        "yanked": false,
                        "core-metadata": {"sha256": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"},
                        "dist-info-metadata": {"sha256": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"},
                        "gpg-sig": false,
                        "provenance": "https://example.test/provenance/peryxpkg-1.0-py3-none-any.whl"
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
