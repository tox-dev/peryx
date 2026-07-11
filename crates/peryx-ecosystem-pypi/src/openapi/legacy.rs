//! The pypi.org-shaped legacy JSON API.

#[allow(clippy::wildcard_imports, reason = "shared is this module's OpenAPI-builder prelude")]
use super::shared::*;

pub(super) fn legacy_project_json() -> OperationBuilder {
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
                        "name": "peryxpkg",
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
                            "filename": "peryxpkg-1.0-py3-none-any.whl",
                            "has_sig": false,
                            "md5_digest": null,
                            "packagetype": "bdist_wheel",
                            "python_version": "py3",
                            "requires_python": ">=3.8",
                            "size": 1832,
                            "upload_time": "2026-01-01T00:00:00",
                            "upload_time_iso_8601": "2026-01-01T00:00:00Z",
                            "url": "/root/pypi/files/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08/peryxpkg-1.0-py3-none-any.whl",
                            "yanked": false,
                            "yanked_reason": null
                        }]
                    },
                    "urls": [{
                        "comment_text": "",
                        "digests": {"sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"},
                        "downloads": -1,
                        "filename": "peryxpkg-1.0-py3-none-any.whl",
                        "has_sig": false,
                        "md5_digest": null,
                        "packagetype": "bdist_wheel",
                        "python_version": "py3",
                        "requires_python": ">=3.8",
                        "size": 1832,
                        "upload_time": "2026-01-01T00:00:00",
                        "upload_time_iso_8601": "2026-01-01T00:00:00Z",
                        "url": "/root/pypi/files/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08/peryxpkg-1.0-py3-none-any.whl",
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
pub(super) fn legacy_release_json() -> OperationBuilder {
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
                        "name": "peryxpkg",
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
                        "filename": "peryxpkg-1.0.tar.gz",
                        "has_sig": false,
                        "md5_digest": null,
                        "packagetype": "sdist",
                        "python_version": "source",
                        "requires_python": ">=3.8",
                        "size": 5760,
                        "upload_time": "2026-01-01T00:00:01",
                        "upload_time_iso_8601": "2026-01-01T00:00:01Z",
                        "url": "/root/pypi/files/0ace7980f82c5815ede4cd7bf9f6693684cec2ae47b9b7ade9add533b8627c6b/peryxpkg-1.0.tar.gz",
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
