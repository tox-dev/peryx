//! The server half: an axum router that renders the app with data read straight from `AppState`,
//! plus the data builders the resource fetchers use during server rendering.

use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes as _, generate_route_list};
use velodex_core::pypi::{CoreMetadataDoc, normalize_name, parse_metadata, to_json};
use velodex_http::{AppState, cache};
use velodex_storage::blob::Digest;

use crate::model::{UiIndex, UiMember, UiMemberChunk, UiProject, UiSnapshot};
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
    let app = expect_context::<Arc<AppState>>();
    let indexes = app
        .describe_indexes()
        .into_iter()
        .map(|index| UiIndex {
            name: index.name,
            route: index.route,
            kind: index.kind.to_owned(),
            layers: index.layers,
            uploads: index.uploads,
            upload_to: index.upload_to,
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
#[must_use]
pub fn projects(route: &str) -> Vec<String> {
    let app = expect_context::<Arc<AppState>>();
    find_index(&app, route)
        .and_then(|index| cache::resolve_list(&app, index).ok())
        .map(|list| list.projects.into_iter().map(|entry| entry.name).collect())
        .unwrap_or_default()
}

/// One project's page data: files plus the parsed core metadata of its newest wheel with a PEP 658
/// sibling.
pub async fn project(route: &str, project: &str) -> Option<(UiProject, Option<CoreMetadataDoc>)> {
    let app = expect_context::<Arc<AppState>>();
    let index = find_index(&app, route)?;
    let normalized = normalize_name(project);
    let detail = cache::resolve_detail(&app, index, &normalized, route).await.ok()??;
    let value = serde_json::from_str(&to_json(&detail)).ok()?;
    let ui = UiProject::from_detail(&value);
    let doc = if let Some(file) = ui.files.iter().rev().find(|file| file.has_metadata)
        && let Some(digest) = Digest::from_hex(&file.sha256)
        && let Ok(bytes) = cache::metadata_bytes(&app, &digest).await
    {
        Some(parse_metadata(&String::from_utf8_lossy(&bytes)))
    } else {
        None
    };
    Some((ui, doc))
}

fn find_index<'a>(app: &'a AppState, route: &str) -> Option<&'a velodex_http::Index> {
    app.indexes.iter().find(|index| index.route == route)
}

/// The member listing of a cached archive, for server rendering.
pub async fn members(route: &str, sha256: &str, filename: &str) -> Vec<UiMember> {
    let app = expect_context::<Arc<AppState>>();
    let Some(digest) = Digest::from_hex(sha256) else {
        return Vec::new();
    };
    let Ok(path) = cache::file_path(app, digest, route.to_owned(), filename.to_owned()).await else {
        return Vec::new();
    };
    let filename = filename.to_owned();
    tokio::task::spawn_blocking(move || velodex_http::archive::list_members_path(&filename, &path))
        .await
        .ok()
        .and_then(Result::ok)
        .map(|members| {
            members
                .into_iter()
                .map(|member| UiMember {
                    path: member.path,
                    size: member.size,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One archive member chunk, for server rendering.
pub async fn member_chunk(route: &str, sha256: &str, filename: &str, member: &str, offset: u64) -> UiMemberChunk {
    let app = expect_context::<Arc<AppState>>();
    let Some(digest) = Digest::from_hex(sha256) else {
        return UiMemberChunk::default();
    };
    let Ok(path) = cache::file_path(app, digest, route.to_owned(), filename.to_owned()).await else {
        return UiMemberChunk::default();
    };
    let filename = filename.to_owned();
    let member = member.to_owned();
    tokio::task::spawn_blocking(move || {
        velodex_http::archive::read_member_chunk_path(
            &filename,
            &path,
            &member,
            offset,
            velodex_http::archive::DEFAULT_MEMBER_CHUNK,
        )
    })
    .await
    .ok()
    .and_then(Result::ok)
    .map_or_else(
        || UiMemberChunk {
            text: "(binary or unavailable)".to_owned(),
            ..UiMemberChunk::default()
        },
        |chunk| UiMemberChunk {
            text: String::from_utf8_lossy(&chunk.bytes).into_owned(),
            size: Some(chunk.size),
            offset: chunk.offset,
            next_offset: chunk.next_offset,
        },
    )
}

/// The stats tree at the requested depth, read from the metrics aggregator.
#[must_use]
pub fn stats(route: Option<&str>, project: Option<&str>) -> serde_json::Value {
    let app = expect_context::<Arc<AppState>>();
    app.metrics.drill(route, project)
}
