//! The per-index `+api` discovery operation, described with a `PyPI` example: the `simple` URLs and
//! the `pip`/`uv`/`.pypirc` client snippets a Python user pastes. The endpoint itself is peryx's own
//! and every ecosystem serves one, but its example document is format-specific, so it lives here
//! rather than in the neutral `OpenAPI` builders.

use super::shared::{OperationBuilder, ResponseBuilder, api_json_response, json, route_param};

pub(super) fn index_discovery() -> OperationBuilder {
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
                        "kind": "virtual",
                        "layers": ["hosted", "pypi"],
                        "uploads": true,
                        "upload_to": "hosted",
                        "capabilities": {
                            "simple_html": true,
                            "simple_json": true,
                            "simple_api_version": "1.4",
                            "metadata_siblings": true,
                            "uploads": true,
                            "yanking": true,
                            "volatile_deletes": true,
                            "project_status": true,
                            "provenance": true,
                            "legacy_json": true
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
                            "uv.toml": "publish-url = \"http://127.0.0.1:4433/root/pypi/\"\n\n[[index]]\nname = \"peryx\"\nurl = \"http://127.0.0.1:4433/root/pypi/simple/\"\ndefault = true\n\n[pip]\nindex-url = \"http://127.0.0.1:4433/root/pypi/simple/\"\n",
                            ".pypirc": "[distutils]\nindex-servers =\n    peryx\n\n[peryx]\nrepository = http://127.0.0.1:4433/root/pypi/\nusername = __token__\npassword = <upload-token>\n"
                        }
                    }
                }),
            ),
        )
        .response("404", ResponseBuilder::new().description("No index at this route"))
}
