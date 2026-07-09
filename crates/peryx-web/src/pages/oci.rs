#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use super::{ErrorMessage, copy_to_clipboard, human_size};
use crate::data::{
    load_oci_layer_chunk, load_oci_layer_members, load_oci_manifest, load_oci_repositories, load_oci_tags,
};
use crate::model::UiOciManifest;
use crate::url::{
    browse_index_url, browse_oci_layer_member_url, browse_oci_layer_url, browse_oci_ref_url, browse_project_url,
};

/// An OCI repository: its tags, each linking to the manifest that tag resolves to.
#[component]
pub(super) fn OciRepositoryView(route: String, repo: String) -> impl IntoView {
    let tags = Resource::new(
        {
            let key = (route.clone(), repo.clone());
            move || key.clone()
        },
        |(route, repo)| load_oci_tags(route, repo),
    );
    let crumb_route = route.clone();
    let crumb_repo = repo.clone();
    view! {
        <p class="breadcrumb">
            <a href=browse_index_url(&crumb_route)>{crumb_route.clone()}</a>
            " / "
            <span>{crumb_repo}</span>
        </p>
        <h1><code>{repo.clone()}</code></h1>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let repo = repo.clone();
                Suspend::new(async move {
                    match tags.await {
                        Ok(tags) if tags.is_empty() => {
                            view! { <p class="dim">"No tags for this repository yet."</p> }.into_any()
                        }
                        Ok(tags) => {
                            view! {
                                <p class="dim">{tags.len()}" tag(s)"</p>
                                <div class="table-scroll">
                                    <table class="files">
                                        <thead><tr><th>"Tag"</th></tr></thead>
                                        <tbody>
                                            {tags
                                                .into_iter()
                                                .map(|tag| {
                                                    let href = browse_oci_ref_url(&route, &repo, &tag);
                                                    view! { <tr><td><a href=href>{tag}</a></td></tr> }
                                                })
                                                .collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                            }
                                .into_any()
                        }
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

/// An OCI index (registry): the repositories it holds, each linking to its tags.
#[component]
pub(super) fn OciIndexView(route: String) -> impl IntoView {
    let repos = Resource::new(
        {
            let route = route.clone();
            move || route.clone()
        },
        load_oci_repositories,
    );
    let heading = route.clone();
    view! {
        <h1><code>{heading}</code></h1>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                Suspend::new(async move {
                    match repos.await {
                        Ok(repos) if repos.is_empty() => {
                            view! { <p class="dim">"No repositories cached or pushed yet."</p> }.into_any()
                        }
                        Ok(repos) => {
                            view! {
                                <ul class="project-list">
                                    {repos
                                        .into_iter()
                                        .map(|repo| {
                                            let href = browse_project_url(&route, &repo);
                                            view! { <li><a href=href>{repo}</a></li> }
                                        })
                                        .collect_view()}
                                </ul>
                            }
                                .into_any()
                        }
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

/// One tag's manifest: the config and layer blobs of an image manifest, or the per-platform child
/// manifests of an image index, each shown by digest and size.
#[component]
pub(super) fn OciManifestView(route: String, repo: String, reference: String) -> impl IntoView {
    let manifest = Resource::new(
        {
            let key = (route.clone(), repo.clone(), reference.clone());
            move || key.clone()
        },
        |(route, repo, reference)| load_oci_manifest(route, repo, reference),
    );
    let crumb_route = route.clone();
    let crumb_repo = repo.clone();
    let crumb_ref = reference.clone();
    view! {
        <p class="breadcrumb">
            <a href=browse_index_url(&crumb_route)>{crumb_route.clone()}</a>
            " / "
            <a href=browse_project_url(&crumb_route, &crumb_repo)>{crumb_repo.clone()}</a>
            " / "
            <span>{crumb_ref}</span>
        </p>
        <h1><code>{repo.clone()}":"{reference.clone()}</code></h1>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let repo = repo.clone();
                let reference = reference.clone();
                Suspend::new(async move {
                    match manifest.await {
                        Ok(Some(manifest)) => {
                            view! { <OciManifestBody route repo reference manifest /> }.into_any()
                        }
                        Ok(None) => view! { <p class="dim">"Manifest not found for this reference."</p> }.into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

/// Render a parsed OCI manifest: its media type and total size, then its config and layers (an image
/// manifest) or its per-platform child manifests (an image index). Each browsable tar layer links to
/// its contents.
#[component]
fn OciManifestBody(route: String, repo: String, reference: String, manifest: UiOciManifest) -> impl IntoView {
    let is_index = manifest.is_index;
    let entry_heading = if is_index { "Platform manifests" } else { "Layers" };
    let pull = view! { <OciPullSnippet route=route.clone() repo=repo.clone() reference=reference.clone() /> };
    let config = manifest.config.map(|config| {
        view! {
            <p><strong>"Config"</strong>": "<code>{config.digest}</code>" ("{human_size(config.size)}")"</p>
        }
    });
    view! {
        <p class="dim">
            <code>{manifest.media_type}</code>" · "{human_size(manifest.total_size)}
        </p>
        {pull}
        {config}
        <h2>{entry_heading}</h2>
        <div class="table-scroll">
            <table class="files">
                <thead>
                    <tr>
                        <th>"Digest"</th>
                        {is_index.then(|| view! { <th>"Platform"</th> })}
                        <th>"Size"</th>
                        <th>"Media type"</th>
                        {(!is_index).then(|| view! { <th>"Contents"</th> })}
                    </tr>
                </thead>
                <tbody>
                    {manifest
                        .entries
                        .into_iter()
                        .map(|entry| {
                            let contents = (!is_index && is_browsable_layer(&entry.media_type)).then(|| {
                                let href = browse_oci_layer_url(&route, &repo, &reference, &entry.digest);
                                view! { <a class="inspect" href=href>"contents"</a> }
                            });
                            view! {
                                <tr>
                                    <td><code>{entry.digest}</code></td>
                                    {is_index
                                        .then(|| view! { <td>{entry.platform.unwrap_or_default()}</td> })}
                                    <td>{human_size(entry.size)}</td>
                                    <td>{entry.media_type}</td>
                                    {(!is_index).then(|| view! { <td>{contents}</td> })}
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </div>
    }
}

/// Whether a layer's media type is a tar the archive engine can list. Config blobs (JSON) and foreign
/// or non-tar layers get no contents link.
fn is_browsable_layer(media_type: &str) -> bool {
    media_type.contains("tar")
}

/// The `docker pull` command for one tag. The registry host is unknown during server rendering, so the
/// snippet ships a `<host>` placeholder that a client-side effect rewrites to the page's own host.
/// Both sides render the placeholder first, so hydration matches.
#[component]
fn OciPullSnippet(route: String, repo: String, reference: String) -> impl IntoView {
    let (host, set_host) = signal("<host>".to_owned());
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            if let Some(window) = web_sys::window()
                && let Ok(host) = window.location().host()
                && !host.is_empty()
            {
                set_host.set(host);
            }
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = set_host;
    let suffix = format!("/{route}/{repo}:{reference}");
    let display = {
        let suffix = suffix.clone();
        move || format!("docker pull {}{suffix}", host.get())
    };
    let copy = move || format!("docker pull {}{suffix}", host.get());
    view! {
        <div class="install">
            <code>{display}</code>
            <button class="copy" title="Copy" on:click=move |_| copy_to_clipboard(&copy())>"copy"</button>
        </div>
    }
}

/// Browse one layer's contents: a flat member listing, or one text member previewed. A layer is a
/// tar, so it drives the same neutral archive engine and member model the wheel browser uses.
#[component]
pub(super) fn OciLayerView(
    route: String,
    repo: String,
    reference: String,
    digest: String,
    member: Option<String>,
    offset: u64,
) -> impl IntoView {
    let manifest = browse_oci_ref_url(&route, &repo, &reference);
    view! {
        <p class="breadcrumb">
            <a href=browse_index_url(&route)>{route.clone()}</a>
            " / "
            <a href=browse_project_url(&route, &repo)>{repo.clone()}</a>
            " / "
            <a href=manifest>{reference.clone()}</a>
            " / "
            <span><code>{digest.clone()}</code></span>
        </p>
        {match member {
            Some(path) => view! { <OciLayerMemberView route repo reference digest member=path offset /> }.into_any(),
            None => view! { <OciLayerMemberList route repo reference digest /> }.into_any(),
        }}
    }
}

#[component]
fn OciLayerMemberList(route: String, repo: String, reference: String, digest: String) -> impl IntoView {
    let members = Resource::new(
        {
            let key = (route.clone(), repo.clone(), digest.clone());
            move || key.clone()
        },
        |(route, repo, digest)| load_oci_layer_members(route, repo, digest),
    );
    view! {
        <h1>"Layer contents"</h1>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let repo = repo.clone();
                let reference = reference.clone();
                let digest = digest.clone();
                Suspend::new(async move {
                    match members.await {
                        Ok(entries) if entries.is_empty() => {
                            view! { <p class="dim">"No files found in this layer."</p> }.into_any()
                        }
                        Ok(entries) => view! {
                            <ul class="archive-tree">
                                {entries
                                    .into_iter()
                                    .map(|entry| {
                                        let name = view! {
                                            <span class="archive-meta">{human_size(entry.size)}" · "{entry.kind.clone()}</span>
                                        };
                                        if entry.previewable {
                                            let href = browse_oci_layer_member_url(&route, &repo, &reference, &digest, &entry.path, 0);
                                            view! { <li><a class="archive-name" href=href>{entry.path}</a>" "{name}</li> }.into_any()
                                        } else {
                                            view! { <li><span class="archive-name">{entry.path}</span>" "{name}</li> }.into_any()
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        }.into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

#[component]
fn OciLayerMemberView(
    route: String,
    repo: String,
    reference: String,
    digest: String,
    member: String,
    offset: u64,
) -> impl IntoView {
    let content = Resource::new(
        {
            let key = (route.clone(), repo.clone(), digest.clone(), member.clone(), offset);
            move || key.clone()
        },
        |(route, repo, digest, member, offset)| load_oci_layer_chunk(route, repo, digest, member, offset),
    );
    let back = browse_oci_layer_url(&route, &repo, &reference, &digest);
    view! {
        <h1><code>{member.clone()}</code></h1>
        <p><a href=back>"back to layer"</a></p>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let repo = repo.clone();
                let reference = reference.clone();
                let digest = digest.clone();
                let member = member.clone();
                Suspend::new(async move {
                    match content.await {
                        Ok(chunk) => {
                            let next = chunk.next_offset.map(|offset| {
                                browse_oci_layer_member_url(&route, &repo, &reference, &digest, &member, offset)
                            });
                            let end = chunk
                                .next_offset
                                .or(chunk.size)
                                .unwrap_or_else(|| chunk.offset + chunk.text.len() as u64);
                            let range = chunk.size.map(|size| {
                                view! { <p class="dim">"bytes "{chunk.offset}"-"{end}" of "{size}</p> }
                            });
                            view! {
                                {range}
                                <pre class="member-content"><code>{chunk.text}</code></pre>
                                {next.map(|href| view! { <p><a class="button-link" href=href>"next chunk"</a></p> })}
                            }.into_any()
                        }
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}
