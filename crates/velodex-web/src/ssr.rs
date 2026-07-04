//! The server half: an axum router that renders the app with data read straight from `AppState`,
//! plus the data builders the resource fetchers use during server rendering.

use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes as _, generate_route_list};
use velodex_ecosystem_pypi::{CoreMetadataDoc, normalize_name, parse_metadata, to_json};
use velodex_http::search::{SearchParams, SourceFilter};
use velodex_http::{AppState, cache};
use velodex_storage::blob::Digest;

use crate::model::{
    UiIndex, UiLocal, UiMember, UiMemberChunk, UiProject, UiRecentUpload, UiSearchPage, UiSnapshot, UiUpstream,
};
use crate::{App, shell};

/// The router state: leptos options plus the velodex application state.
#[derive(Clone)]
pub struct UiState {
    pub options: LeptosOptions,
    pub app: Arc<AppState>,
}

impl FromRef<UiState> for LeptosOptions {
    fn from_ref(state: &UiState) -> Self {
        state.options.clone()
    }
}

/// Build the UI router.
///
/// The leptos routes (server-rendered, hydration-ready) plus the `/pkg` asset directory holding
/// the wasm bundle. Without the bundle on disk the pages still render; they are just not
/// interactive.
pub fn ui_router(app: Arc<AppState>) -> Router {
    let options = leptos_options();
    let site_root = options.site_root.to_string();
    let state = UiState { options, app };
    let routes = generate_route_list(App);
    Router::new()
        .leptos_routes_with_context(
            &state,
            routes,
            {
                let app = state.app.clone();
                move || provide_context(app.clone())
            },
            {
                let options = state.options.clone();
                move || shell(options.clone())
            },
        )
        // leptos appends `_bg` to the wasm name when the server was not compiled by cargo-leptos
        // (a compile-time env probe), while cargo-leptos writes the file without it; alias the two.
        .route_service(
            "/pkg/velodex_web_bg.wasm",
            tower_http::services::ServeFile::new(format!("{site_root}/pkg/velodex_web.wasm")),
        )
        .nest_service("/pkg", tower_http::services::ServeDir::new(format!("{site_root}/pkg")))
        .with_state(state)
}

/// The leptos configuration: asset names must match what cargo-leptos produces (`Cargo.toml`
/// workspace metadata), and the site root is where its output lands at runtime.
fn leptos_options() -> LeptosOptions {
    LeptosOptions::builder()
        .output_name("velodex_web")
        .site_root("ui")
        .site_pkg_dir("pkg")
        .build()
}

/// The dashboard snapshot, read from `AppState`.
#[must_use]
pub fn snapshot() -> UiSnapshot {
    snapshot_with_summaries(None)
}

/// The richer admin status snapshot.
#[must_use]
pub fn admin_snapshot() -> UiSnapshot {
    snapshot_with_summaries(Some(5))
}

fn snapshot_with_summaries(recent_limit: Option<usize>) -> UiSnapshot {
    let app = expect_context::<Arc<AppState>>();
    let summaries = recent_limit.map(|limit| {
        let index_names = app.indexes.iter().map(|index| index.name.clone()).collect::<Vec<_>>();
        app.meta.summarize_indexes(&index_names, limit).unwrap_or_default()
    });
    let indexes = app
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let summary = summaries
                .as_ref()
                .and_then(|summaries| summaries.get(&index.name))
                .cloned()
                .unwrap_or_default();
            UiIndex {
                name: index.name,
                route: index.route,
                kind: index.kind.to_owned(),
                layers: index.layers,
                uploads: index.uploads,
                upload_to: index.upload_to,
                upstream: index.upstream.map(|upstream| UiUpstream {
                    url: upstream.url,
                    auth_kind: upstream.auth.to_owned(),
                    auth_redacted: (upstream.auth != "none").then(|| "<redacted>".to_owned()),
                    status: "configured".to_owned(),
                }),
                local: index.local.map(|local| UiLocal {
                    volatile: local.volatile,
                    token_configured: local.upload_token.configured,
                    token_redacted: local.upload_token.redacted.map(str::to_owned),
                }),
                project_count: summary.project_count,
                upload_count: summary.upload_count,
                recent_uploads: summary
                    .recent_uploads
                    .into_iter()
                    .map(|upload| UiRecentUpload {
                        project: upload.project,
                        filename: upload.filename,
                        version: upload.version,
                        uploaded_at: upload.uploaded_at,
                        size: upload.size,
                    })
                    .collect(),
            }
        })
        .collect();
    UiSnapshot {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        serial: app.meta.current_serial().unwrap_or(0),
        requests: app.requests.load(std::sync::atomic::Ordering::Relaxed),
        metadata_requests: app.metadata_requests.load(std::sync::atomic::Ordering::Relaxed),
        indexes,
    }
}

/// The project names of the index at `route`.
///
/// # Errors
/// Returns a user-visible message when the index is unknown or its project list cannot be read.
pub fn projects(route: &str) -> Result<Vec<String>, String> {
    let app = expect_context::<Arc<AppState>>();
    let Some(index) = find_index(&app, route) else {
        return Err(format!("index {route:?} is not configured"));
    };
    cache::resolve_list(&app, index)
        .map(|list| list.projects.into_iter().map(|entry| entry.name).collect())
        .map_err(|err| format!("project list on index {route:?}: {}", err.user_message()))
}

/// One project's page data: files plus the parsed core metadata of its newest wheel with a PEP 658
/// sibling.
///
/// # Errors
/// Returns a user-visible message when project detail or metadata cannot be read.
pub async fn project(route: &str, project: &str) -> Result<Option<(UiProject, Option<CoreMetadataDoc>)>, String> {
    let app = expect_context::<Arc<AppState>>();
    let Some(index) = find_index(&app, route) else {
        return Err(format!("index {route:?} is not configured"));
    };
    let normalized = normalize_name(project);
    let Some(detail) = cache::resolve_detail(&app, index, &normalized, route)
        .await
        .map_err(|err| {
            format!(
                "project detail on index {route:?} for project {normalized:?}: {}",
                err.user_message()
            )
        })?
    else {
        return Ok(None);
    };
    let value = serde_json::from_str(&to_json(&detail))
        .map_err(|err| format!("project detail on index {route:?} for project {normalized:?}: {err}"))?;
    let ui = UiProject::from_detail(&value);
    let doc = match ui.files.iter().rev().find(|file| file.has_metadata) {
        Some(file) => {
            let Some(digest) = Digest::from_hex(&file.sha256) else {
                return Err(format!(
                    "metadata fetch on index {route:?} for file {:?}: invalid sha256 digest {:?}",
                    file.filename, file.sha256
                ));
            };
            let metadata_filename = format!("{}.metadata", file.filename);
            let bytes = cache::metadata_bytes(&app, &digest, route, &metadata_filename)
                .await
                .map_err(|err| {
                    format!(
                        "metadata fetch on index {route:?} for file {:?} with digest {}: {}",
                        file.filename,
                        digest.as_str(),
                        err.user_message()
                    )
                })?;
            Some(parse_metadata(&String::from_utf8_lossy(&bytes)))
        }
        None => None,
    };
    Ok(Some((ui, doc)))
}

/// Search cached packages during server rendering.
///
/// # Errors
/// Returns a user-visible message when search fails.
pub fn search(query: &str, source_type: &str, page: usize, page_size: usize) -> Result<UiSearchPage, String> {
    let app = expect_context::<Arc<AppState>>();
    let params = SearchParams {
        query: query.to_owned(),
        route: None,
        source: SourceFilter::from_value(source_type).unwrap_or(SourceFilter::All),
        page: page.max(1),
        page_size: match page_size {
            25 | 50 | 100 => page_size,
            _ => 25,
        },
    };
    let response = app
        .search
        .search(&app, params)
        .map_err(|err| format!("package search: {err}"))?;
    let value = serde_json::to_value(response).map_err(|err| format!("search result: {err}"))?;
    Ok(UiSearchPage::from_search(&value))
}

fn find_index<'a>(app: &'a AppState, route: &str) -> Option<&'a velodex_http::Index> {
    app.indexes.iter().find(|index| index.route == route)
}

/// The member listing of a cached archive, for server rendering.
///
/// # Errors
/// Returns a user-visible message when the artifact cannot be found, fetched, or listed.
pub async fn members(
    route: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
) -> Result<Vec<UiMember>, String> {
    let app = expect_context::<Arc<AppState>>();
    let Some(digest) = Digest::from_hex(sha256) else {
        return Err(format!(
            "archive listing on index {route:?} for file {filename:?}: invalid sha256 digest {sha256:?}"
        ));
    };
    let path = cache::file_path(app, digest, route.to_owned(), filename.to_owned())
        .await
        .map_err(|err| {
            format!(
                "archive listing on index {route:?} for file {filename:?} with digest {sha256}: {}",
                err.user_message()
            )
        })?;
    let archive = filename.to_owned();
    let containers = containers.to_vec();
    let members = tokio::task::spawn_blocking(move || {
        velodex_http::archive::list_members_nested_path(&archive, &path, &containers)
    })
    .await
    .map_err(|err| format!("archive listing on index {route:?} for file {filename:?}: {err}"))?
    .map_err(|err| format!("archive listing on index {route:?} for file {filename:?}: {err}"))?;
    Ok(members
        .into_iter()
        .map(|member| UiMember {
            path: member.path,
            size: member.size,
            kind: member.kind.as_str().to_owned(),
            previewable: member.previewable,
        })
        .collect())
}

/// One archive member chunk, for server rendering.
///
/// # Errors
/// Returns a user-visible message when the member cannot be previewed as UTF-8 text.
pub async fn member_chunk(
    route: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
    member: &str,
    offset: u64,
) -> Result<UiMemberChunk, String> {
    let app = expect_context::<Arc<AppState>>();
    let Some(digest) = Digest::from_hex(sha256) else {
        return Err(format!(
            "archive member on index {route:?} for file {filename:?}: invalid sha256 digest {sha256:?}"
        ));
    };
    let path = cache::file_path(app, digest, route.to_owned(), filename.to_owned())
        .await
        .map_err(|err| {
            format!(
                "archive member on index {route:?} for file {filename:?} with digest {sha256}: {}",
                err.user_message()
            )
        })?;
    let archive = filename.to_owned();
    let containers = containers.to_vec();
    let selected = member.to_owned();
    let chunk = tokio::task::spawn_blocking(move || {
        velodex_http::archive::read_text_member_chunk_nested_path(
            &archive,
            &path,
            &containers,
            &selected,
            offset,
            velodex_http::archive::DEFAULT_MEMBER_CHUNK,
        )
    })
    .await
    .map_err(|err| format!("archive member {member:?} on index {route:?} for file {filename:?}: {err}"))?
    .map_err(|err| format!("archive member {member:?} on index {route:?} for file {filename:?}: {err}"))?;
    Ok(UiMemberChunk {
        text: String::from_utf8(chunk.bytes).map_err(|err| {
            format!("archive member {member:?} on index {route:?} for file {filename:?} is not valid UTF-8: {err}")
        })?,
        size: Some(chunk.size),
        offset: chunk.offset,
        next_offset: chunk.next_offset,
    })
}

/// The stats tree at the requested depth, read from the metrics aggregator.
#[must_use]
pub fn stats(route: Option<&str>, project: Option<&str>) -> serde_json::Value {
    let app = expect_context::<Arc<AppState>>();
    app.metrics.drill(route, project)
}
