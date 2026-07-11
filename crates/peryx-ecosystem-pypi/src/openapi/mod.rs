//! The `OpenAPI` description of the `PyPI` Simple API, legacy JSON, file, inspect and publish
//! operations this driver serves, one submodule per surface.

use utoipa::openapi::PathsBuilder;
use utoipa::openapi::path::{HttpMethod, PathItemBuilder};

use peryx_driver::openapi::package_search;

mod discovery;
mod files;
mod inspect;
mod legacy;
mod publish;
mod shared;
mod simple;

use discovery::index_discovery;
use files::{file_download, metadata_download};
use inspect::{inspect_listing, inspect_member};
use legacy::{legacy_project_json, legacy_release_json};
use publish::{delete_project, delete_version, promote, restore, unyank, upload, yank};
use simple::{project_detail, project_list};

/// Every path a `PyPI` index serves, mounted under its route. The composition root folds each
/// ecosystem's paths into one document.
#[must_use]
pub fn openapi_paths(paths: PathsBuilder) -> PathsBuilder {
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
