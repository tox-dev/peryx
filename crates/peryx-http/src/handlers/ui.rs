//! `GET /+ui/…`: neutral browse-view data for the hydrated web UI.
//!
//! Each endpoint resolves the index route to its ecosystem driver and returns the driver-produced
//! neutral view model as plain JSON, so the browser never links an ecosystem crate or parses a format
//! API — it fetches these and deserializes into the shared view models.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use peryx_driver::serving::EcosystemDriver;
use peryx_driver::state::AppState;

#[derive(Debug, serde::Deserialize)]
pub struct IndexQuery {
    index: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct ProjectQuery {
    index: String,
    project: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct ManifestQuery {
    index: String,
    project: String,
    #[serde(rename = "ref")]
    reference: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct MembersQuery {
    index: String,
    project: String,
    digest: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct MemberQuery {
    index: String,
    project: String,
    digest: String,
    member: String,
    #[serde(default)]
    offset: u64,
}

/// `GET /+ui/projects?index=<route>`: the project names of one index.
pub async fn ui_projects(State(state): State<Arc<AppState>>, Query(query): Query<IndexQuery>) -> Response {
    let Some((position, driver)) = resolve(&state, &query.index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match driver.project_names(&state.serving, position) {
        Ok(names) => axum::Json(names).into_response(),
        Err(message) => server_error(&message),
    }
}

/// `GET /+ui/project?index=<route>&project=<name>`: one project's browse view, `404` when absent.
pub async fn ui_project(State(state): State<Arc<AppState>>, Query(query): Query<ProjectQuery>) -> Response {
    let Some((position, driver)) = resolve(&state, &query.index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match driver
        .browse_project(state.serving.clone(), position, query.project)
        .await
    {
        Ok(Some(view)) => axum::Json(view).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(message) => server_error(&message),
    }
}

/// `GET /+ui/manifest?index=<route>&project=<repo>&ref=<reference>`: a manifest view, `404` when
/// the reference is not served.
pub async fn ui_manifest(State(state): State<Arc<AppState>>, Query(query): Query<ManifestQuery>) -> Response {
    let Some((position, driver)) = resolve(&state, &query.index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match driver
        .manifest_view(state.serving.clone(), position, query.project, query.reference)
        .await
    {
        Ok(Some(view)) => axum::Json(view).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(message) => server_error(&message),
    }
}

/// `GET /+ui/members?index=<route>&project=<repo>&digest=<digest>`: a nested content item's members.
pub async fn ui_members(State(state): State<Arc<AppState>>, Query(query): Query<MembersQuery>) -> Response {
    let Some((position, driver)) = resolve(&state, &query.index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match driver
        .artifact_members(state.serving.clone(), position, query.project, query.digest)
        .await
    {
        Ok(members) => axum::Json(members).into_response(),
        Err(message) => server_error(&message),
    }
}

/// `GET /+ui/member?index=<route>&project=<repo>&digest=<digest>&member=<m>&offset=<o>`: one text
/// chunk of a nested content member.
pub async fn ui_member(State(state): State<Arc<AppState>>, Query(query): Query<MemberQuery>) -> Response {
    let Some((position, driver)) = resolve(&state, &query.index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match driver
        .artifact_member_chunk(
            state.serving.clone(),
            position,
            query.project,
            query.digest,
            query.member,
            query.offset,
        )
        .await
    {
        Ok(chunk) => axum::Json(chunk).into_response(),
        Err(message) => server_error(&message),
    }
}

/// Resolve an index route to its position and ecosystem driver, or `None` when the route is unknown.
fn resolve(state: &AppState, route: &str) -> Option<(usize, Arc<dyn EcosystemDriver>)> {
    let position = state.indexes.iter().position(|index| index.route == route)?;
    let driver = state.driver_for(state.index_at(position).ecosystem)?.clone();
    Some((position, driver))
}

fn server_error(message: &str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, message.to_owned()).into_response()
}
