#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use super::ErrorMessage;
use super::archive::{ArchiveView, split_legacy_archive_file};
use super::manifest::{LayerView, ManifestView};
use super::project::ProjectView;
use crate::data::load_projects;
use crate::url::browse_project_url;

/// The browse page: a searchable project list, one project's detail, a manifest, or an archive's or
/// layer's contents, selected by query parameters.
///
/// The dispatch is ecosystem-neutral: a `project` renders as whatever [`ProjectView`] gets back from
/// the index's driver (a file listing or a reference list), and the `ref`/`layer` parameters — which
/// only a registry's own URLs carry — select the manifest and layer views. No branch names a format.
#[component]
pub fn Browse() -> impl IntoView {
    let query = use_query_map();
    let route = Memo::new(move |_| query.read().get("index").unwrap_or_default());
    let project = Memo::new(move |_| query.read().get("project").filter(|name| !name.is_empty()));
    let reference = Memo::new(move |_| query.read().get("ref").filter(|name| !name.is_empty()));
    let file = Memo::new(move |_| query.read().get("file").filter(|name| !name.is_empty()));
    let sha256 = Memo::new(move |_| query.read().get("sha256").filter(|digest| !digest.is_empty()));
    let layer = Memo::new(move |_| query.read().get("layer").filter(|digest| !digest.is_empty()));
    let member = Memo::new(move |_| query.read().get("member").filter(|name| !name.is_empty()));
    let containers = Memo::new(move |_| {
        query
            .read()
            .get_all("container")
            .unwrap_or_default()
            .into_iter()
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
    });
    let offset = Memo::new(move |_| {
        query
            .read()
            .get("offset")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default()
    });
    view! {
        <section class="page">
            {move || {
                let route = route.get();
                let project = project.get();
                let reference = reference.get();
                let sha256 = sha256.get();
                let layer = layer.get();
                let file = file.get();
                let member = member.get();
                let containers = containers.get();
                let offset = offset.get();
                match (project, sha256, file, reference) {
                    (Some(name), Some(sha256), Some(file), _) => {
                        view! {
                            <ArchiveView route project=name sha256 filename=file containers member offset />
                        }.into_any()
                    }
                    (Some(name), None, Some(file), _) => {
                        let (sha256, filename) = split_legacy_archive_file(&file);
                        view! {
                            <ArchiveView route project=name sha256 filename containers member offset />
                        }.into_any()
                    }
                    (Some(repo), _, None, Some(reference)) if layer.is_some() => {
                        let digest = layer.unwrap_or_default();
                        view! { <LayerView route repo reference digest member offset /> }.into_any()
                    }
                    (Some(repo), _, None, Some(reference)) => {
                        view! { <ManifestView route repo reference /> }.into_any()
                    }
                    (Some(name), _, None, _) => view! { <ProjectView route project=name /> }.into_any(),
                    (None, _, _, _) => view! { <IndexView route /> }.into_any(),
                }
            }}
        </section>
    }
}

#[component]
fn IndexView(route: String) -> impl IntoView {
    let projects = Resource::new(
        {
            let route = route.clone();
            move || route.clone()
        },
        load_projects,
    );
    let (filter, set_filter) = signal(String::new());
    let heading = route.clone();
    view! {
        <h1><code>{heading}</code></h1>
        <input
            class="search"
            type="search"
            placeholder="Filter projects…"
            on:input:target=move |event| set_filter.set(event.target().value())
        />
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                Suspend::new(async move {
                    match projects.await {
                        Ok(names) => view! { <ProjectList route names filter /> }.into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

#[component]
fn ProjectList(route: String, names: Vec<String>, filter: ReadSignal<String>) -> impl IntoView {
    let empty = names.is_empty();
    view! {
        <ul class="project-list">
            {move || {
                let needle = filter.get().to_lowercase();
                names
                    .iter()
                    .filter(|name| needle.is_empty() || name.to_lowercase().contains(&needle))
                    .map(|name| {
                        let href = browse_project_url(&route, name);
                        view! { <li><a href=href>{name.clone()}</a></li> }
                    })
                    .collect_view()
            }}
        </ul>
        {empty.then(|| view! { <p class="dim">"No projects observed on this index yet."</p> })}
    }
}
