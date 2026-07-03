//! The UI pages: a live dashboard and a pypi.org-style package browser.
#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]
#![allow(
    clippy::missing_const_for_fn,
    reason = "cfg-split helpers are const only without the hydrate feature; constness cannot vary by cfg"
)]

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;
use velodex_core::pypi::CoreMetadataDoc;

use crate::data::{load_member_chunk, load_members, load_project, load_projects, load_snapshot, load_stats};
use crate::markdown::render_description;
use crate::model::{UiCounters, UiFile, UiIndex, UiMember, UiMemberChunk, UiProject, UiSnapshot, UiStats};
use crate::url::encode_component as url_encode;

/// The landing dashboard: identity, live counters, and the configured indexes with their usage.
#[component]
pub fn Dashboard() -> impl IntoView {
    let snapshot = Resource::new(|| (), |()| load_snapshot());
    let stats = Resource::new(|| (), |()| load_stats(None, None));
    start_refresh(snapshot);
    view! {
        <section class="page">
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || Suspend::new(async move {
                    let data = snapshot.await;
                    let usage = stats.await;
                    view! { <DashboardBody data usage /> }
                })}
            </Suspense>
        </section>
    }
}

/// Refresh the dashboard counters every few seconds once hydrated. Effects never run during server
/// rendering, so this is inert in SSR output.
fn start_refresh(snapshot: Resource<UiSnapshot>) {
    #[cfg(feature = "hydrate")]
    {
        use std::time::Duration;
        Effect::new(move |_| {
            set_interval(move || snapshot.refetch(), Duration::from_secs(5));
        });
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = snapshot;
    }
}

#[component]
fn DashboardBody(data: UiSnapshot, usage: UiStats) -> impl IntoView {
    let layered: std::collections::HashSet<String> = data
        .indexes
        .iter()
        .flat_map(|index| index.layers.iter().cloned())
        .collect();
    let counters_for = move |route: &str| {
        usage
            .rows
            .iter()
            .find(|(candidate, _)| candidate == route)
            .map(|(_, counters)| *counters)
    };
    let all = data.indexes.clone();
    let overlay_cards = data
        .indexes
        .iter()
        .filter(|index| !index.layers.is_empty())
        .cloned()
        .map(|index| {
            let counters = counters_for(&index.route);
            view! { <OverlayCard index all=all.clone() counters /> }
        })
        .collect_view();
    let standalone: Vec<UiIndex> = data
        .indexes
        .iter()
        .filter(|index| index.layers.is_empty() && !layered.contains(&index.name))
        .cloned()
        .collect();
    let standalone_cards = (!standalone.is_empty()).then(|| {
        view! {
            <h2>"Standalone indexes"</h2>
            <div class="index-grid">
                {standalone
                    .into_iter()
                    .map(|index| {
                        let counters = counters_for(&index.route);
                        view! { <IndexCard index counters /> }
                    })
                    .collect_view()}
            </div>
        }
    });
    view! {
        <div class="stat-row">
            <div class="stat"><strong>{data.version.clone()}</strong><span>"version"</span></div>
            <div class="stat"><strong>{data.serial}</strong><span>"change serial"</span></div>
            <div class="stat"><strong>{data.requests}</strong><span>"requests served"</span></div>
            <div class="stat"><strong>{data.metadata_requests}</strong><span>"PEP 658 metadata hits"</span></div>
        </div>
        <h2>"Indexes"</h2>
        <div class="index-grid">{overlay_cards}</div>
        {standalone_cards}
    }
}

/// An overlay drawn as what it is: an ordered stack of layers under one route, resolved top to
/// bottom with the first file match winning.
#[component]
fn OverlayCard(index: UiIndex, all: Vec<UiIndex>, counters: Option<UiCounters>) -> impl IntoView {
    let browse = format!("/browse?index={}", url_encode(&index.route));
    let stats_href = format!("/stats?index={}", url_encode(&index.route));
    let simple = format!("/{}/simple/", index.route);
    let upload_to = index.upload_to.clone();
    let layers = index
        .layers
        .iter()
        .enumerate()
        .map(|(position, name)| {
            let member = all.iter().find(|candidate| candidate.name == *name).cloned();
            let kind = member
                .as_ref()
                .map_or_else(|| "?".to_owned(), |member| member.kind.clone());
            let route = member.as_ref().map(|member| format!("/{}/simple/", member.route));
            let is_upload_target = upload_to.as_deref() == Some(name.as_str());
            view! {
                <li class="layer">
                    <span class="layer-order">{position + 1}</span>
                    <span class="layer-name">{name.clone()}</span>
                    <span class=format!("badge kind-{kind}")>{kind.clone()}</span>
                    {is_upload_target
                        .then(|| view! { <span class="badge uploads">"uploads land here"</span> })}
                    {route.map(|route| view! { <code class="layer-route">{route}</code> })}
                </li>
            }
        })
        .collect_view();
    let usage = counters.map(|c| {
        view! {
            <p class="card-usage">
                <span>{c.pages}" pages"</span>
                <span>{c.downloads}" downloads"</span>
                <span>{human_size(c.bytes)}" served"</span>
                <a href=stats_href.clone()>"usage"</a>
            </p>
        }
    });
    view! {
        <div class="card overlay-card">
            <div class="card-head">
                <a href=browse class="card-title">{index.name.clone()}</a>
                <span class="badge kind-overlay">"overlay"</span>
                {index.uploads.then(|| view! { <span class="badge uploads">"uploads"</span> })}
            </div>
            <p class="dim"><code>{simple}</code></p>
            <ol class="layer-stack">{layers}</ol>
            <p class="layer-hint">"resolves top to bottom; first file match wins"</p>
            {usage}
        </div>
    }
}

#[component]
fn IndexCard(index: UiIndex, counters: Option<UiCounters>) -> impl IntoView {
    let browse = format!("/browse?index={}", url_encode(&index.route));
    let stats_href = format!("/stats?index={}", url_encode(&index.route));
    let simple = format!("/{}/simple/", index.route);
    let layers = (!index.layers.is_empty()).then(|| {
        view! {
            <p class="layers">
                "layers: "
                {index.layers.iter().map(|layer| view! { <code>{layer.clone()}</code> }).collect_view()}
            </p>
        }
    });
    let usage = counters.map(|c| {
        view! {
            <p class="card-usage">
                <span>{c.pages}" pages"</span>
                <span>{c.downloads}" downloads"</span>
                <span>{human_size(c.bytes)}" served"</span>
                <a href=stats_href.clone()>"usage"</a>
            </p>
        }
    });
    view! {
        <div class="card">
            <div class="card-head">
                <a href=browse class="card-title">{index.name.clone()}</a>
                <span class=format!("badge kind-{}", index.kind)>{index.kind.clone()}</span>
                {index.uploads.then(|| view! { <span class="badge uploads">"uploads"</span> })}
            </div>
            <p class="dim"><code>{simple}</code></p>
            {layers}
            {usage}
        </div>
    }
}

/// The browse page: a searchable project list, one project's detail, or an archive's contents,
/// selected by query parameters.
#[component]
pub fn Browse() -> impl IntoView {
    let query = use_query_map();
    let route = Memo::new(move |_| query.read().get("index").unwrap_or_default());
    let project = Memo::new(move |_| query.read().get("project").filter(|name| !name.is_empty()));
    let file = Memo::new(move |_| query.read().get("file").filter(|name| !name.is_empty()));
    let sha256 = Memo::new(move |_| query.read().get("sha256").filter(|digest| !digest.is_empty()));
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
            {move || match (project.get(), sha256.get(), file.get()) {
                (Some(name), Some(sha256), Some(file)) => {
                    view! {
                        <ArchiveView
                            route=route.get()
                            project=name
                            sha256
                            filename=file
                            containers=containers.get()
                            member=member.get()
                            offset=offset.get()
                        />
                    }.into_any()
                }
                (Some(name), None, Some(file)) => {
                    let (sha256, filename) = split_legacy_archive_file(&file);
                    view! {
                        <ArchiveView
                            route=route.get()
                            project=name
                            sha256
                            filename
                            containers=containers.get()
                            member=member.get()
                            offset=offset.get()
                        />
                    }.into_any()
                }
                (Some(name), _, None) => view! { <ProjectView route=route.get() project=name /> }.into_any(),
                (None, _, _) => view! { <IndexView route=route.get() /> }.into_any(),
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
                    let names = projects.await;
                    view! { <ProjectList route names filter /> }
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
                    .filter(|name| name.to_lowercase().contains(&needle))
                    .map(|name| {
                        let href =
                            format!("/browse?index={}&project={}", url_encode(&route), url_encode(name));
                        view! { <li><a href=href>{name.clone()}</a></li> }
                    })
                    .collect_view()
            }}
        </ul>
        {empty.then(|| view! { <p class="dim">"No projects observed on this index yet."</p> })}
    }
}

#[component]
fn ProjectView(route: String, project: String) -> impl IntoView {
    let page = Resource::new(
        {
            let key = (route.clone(), project.clone());
            move || key.clone()
        },
        |(route, project)| load_project(route, project),
    );
    // Admin state lives here, outside the Suspend scope: signals created inside async-hydrated
    // suspense content are disposed once hydration completes, which would make them inert.
    let (token, set_token) = signal(String::new());
    let (outcome, set_outcome) = signal(String::new());
    view! {
        <p class="breadcrumb">
            <a href=format!("/browse?index={}", url_encode(&route))>{route.clone()}</a>
            " / "
            <span>{project}</span>
        </p>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                Suspend::new(async move {
                    match page.await {
                        Some((ui, doc)) => {
                            view! { <ProjectBody route ui doc refresh=page token set_token set_outcome /> }
                                .into_any()
                        }
                        None => view! { <p class="dim">"Project not found on this index."</p> }.into_any(),
                    }
                })
            }}
        </Suspense>
        {move || {
            let text = outcome.get();
            (!text.is_empty()).then(|| view! { <p class="outcome">{text}</p> })
        }}
    }
}

#[component]
fn ProjectBody(
    route: String,
    ui: UiProject,
    doc: Option<CoreMetadataDoc>,
    refresh: Resource<Option<(UiProject, Option<CoreMetadataDoc>)>>,
    token: ReadSignal<String>,
    set_token: WriteSignal<String>,
    set_outcome: WriteSignal<String>,
) -> impl IntoView {
    let doc = doc.unwrap_or_default();
    let latest = ui.versions.last().cloned().unwrap_or_else(|| doc.version.clone());
    let install = format!("uv pip install --index-url /{route}/simple/ {}", ui.name);
    let description = render_description(&doc);
    view! {
        <header class="project-head">
            <h1>{ui.name.clone()} <span class="version">{latest}</span></h1>
            {doc.summary.clone().map(|summary| view! { <p class="summary">{summary}</p> })}
            <InstallSnippet install />
        </header>
        <div class="project-grid">
            <div class="project-main">
                <h2>"Description"</h2>
                <div class="description" inner_html=description></div>
                <h2>"Files"</h2>
                <FileTable route=route.clone() project=ui.name.clone() files=ui.files.clone() />
                <AdminPanel route=route project=ui.name.clone() versions=ui.versions.clone() refresh token set_token set_outcome />
            </div>
            <aside class="project-side">
                <MetaPanel doc versions=ui.versions />
            </aside>
        </div>
    }
}

#[component]
fn InstallSnippet(install: String) -> impl IntoView {
    let shown = install.clone();
    view! {
        <div class="install">
            <code>{shown}</code>
            <button class="copy" title="Copy" on:click=move |_| copy_to_clipboard(&install)>"copy"</button>
        </div>
    }
}

fn copy_to_clipboard(text: &str) {
    #[cfg(feature = "hydrate")]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().clipboard().write_text(text);
        }
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = text;
    }
}

#[component]
fn FileTable(route: String, project: String, files: Vec<UiFile>) -> impl IntoView {
    view! {
        <table class="files">
            <thead><tr><th>"File"</th><th>"Size"</th><th>"Uploaded"</th><th>"sha256"</th><th>"Flags"</th></tr></thead>
            <tbody>
                {files
                    .into_iter()
                    .map(|file| {
                        let class = if file.yanked { "yanked" } else { "" };
                        let inspect = format!(
                            "/browse?index={}&project={}&sha256={}&file={}",
                            url_encode(&route),
                            url_encode(&project),
                            url_encode(&file.sha256),
                            url_encode(&file.filename)
                        );
                        let short_hash = file.sha256.get(..12).unwrap_or_default().to_owned();
                        view! {
                            <tr class=class>
                                <td>
                                    <a href=file.url.clone()>{file.filename.clone()}</a>
                                    {supports_archive_browser(&file.filename)
                                        .then(|| view! {
                                            " · "
                                            <a class="inspect" href=inspect>"contents"</a>
                                        })}
                                </td>
                                <td>{file.size.map_or_else(|| "—".to_owned(), human_size)}</td>
                                <td>{file.upload_time.clone().map_or_else(|| "—".to_owned(), |t| t.chars().take(10).collect())}</td>
                                <td><code title=file.sha256.clone()>{short_hash}</code></td>
                                <td>
                                    {file.yanked.then(|| view! { <span class="badge yanked-badge">"yanked"</span> })}
                                    {file
                                        .has_metadata
                                        .then(|| view! { <span class="badge meta-badge">"metadata"</span> })}
                                </td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

fn supports_archive_browser(filename: &str) -> bool {
    let path = std::path::Path::new(filename);
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl") || ext.eq_ignore_ascii_case("zip"))
        || filename
            .get(filename.len().saturating_sub(7)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
}

/// The archive browser: member listing of one distribution, or one member's content.
#[component]
fn ArchiveView(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: Option<String>,
    offset: u64,
) -> impl IntoView {
    let back = format!("/browse?index={}&project={}", url_encode(&route), url_encode(&project));
    view! {
        <p class="breadcrumb">
            <a href=format!("/browse?index={}", url_encode(&route))>{route.clone()}</a>
            " / "
            <a href=back>{project.clone()}</a>
            " / "
            <ArchiveBreadcrumb route=route.clone() project=project.clone() sha256=sha256.clone() filename=filename.clone() containers=containers.clone() />
        </p>
        {match member {
            Some(path) => {
                view! { <MemberView route project sha256 filename containers member=path offset /> }.into_any()
            }
            None => view! { <MemberList route project sha256 filename containers /> }.into_any(),
        }}
    }
}

#[component]
fn ArchiveBreadcrumb(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
) -> impl IntoView {
    let root = archive_listing_href(&route, &project, &sha256, &filename, &[]);
    let filename_view = if containers.is_empty() {
        view! { <span>{filename.clone()}</span> }.into_any()
    } else {
        view! { <a href=root>{filename.clone()}</a> }.into_any()
    };
    view! {
        {filename_view}
        {containers
            .iter()
            .enumerate()
            .map(|(position, container)| {
                let next = position + 1;
                let prefix = containers[..next].to_vec();
                let href = archive_listing_href(&route, &project, &sha256, &filename, &prefix);
                let container = container.clone();
                if next == containers.len() {
                    view! { " / " <span>{container}</span> }.into_any()
                } else {
                    view! { " / " <a href=href>{container}</a> }.into_any()
                }
            })
            .collect_view()}
    }
}

#[component]
fn MemberList(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
) -> impl IntoView {
    let members = Resource::new(
        {
            let key = (route.clone(), sha256.clone(), filename.clone(), containers.clone());
            move || key.clone()
        },
        |(route, sha256, filename, containers)| load_members(route, sha256, filename, containers),
    );
    let heading = containers.last().cloned().unwrap_or_else(|| filename.clone());
    view! {
        <h1><code>{heading}</code></h1>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let project = project.clone();
                let sha256 = sha256.clone();
                let filename = filename.clone();
                let containers = containers.clone();
                Suspend::new(async move {
                    let entries = members.await;
                    view! { <ArchiveTree route project sha256 filename containers entries /> }
                })
            }}
        </Suspense>
    }
}

#[component]
fn ArchiveTree(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    entries: Vec<UiMember>,
) -> impl IntoView {
    let nodes = archive_tree(entries);
    if nodes.is_empty() {
        return view! { <p class="dim">"No files found in this archive."</p> }.into_any();
    }
    view! {
        <ul class="archive-tree">
            {nodes
                .into_iter()
                .map(|node| {
                    view! {
                        <ArchiveTreeNode
                            route=route.clone()
                            project=project.clone()
                            sha256=sha256.clone()
                            filename=filename.clone()
                            containers=containers.clone()
                            node
                        />
                    }
                })
                .collect_view()}
        </ul>
    }
    .into_any()
}

#[component]
fn ArchiveTreeNode(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    node: ArchiveNode,
) -> impl IntoView {
    let ArchiveNode {
        name,
        path,
        size,
        kind,
        previewable,
        directory,
        children,
    } = node;
    if directory {
        return view! {
            <li>
                <details open>
                    <summary><span class="archive-name folder">{name}</span></summary>
                    <ul>
                        {children
                            .into_iter()
                            .map(|child| {
                                view! {
                                    <ArchiveTreeNode
                                        route=route.clone()
                                        project=project.clone()
                                        sha256=sha256.clone()
                                        filename=filename.clone()
                                        containers=containers.clone()
                                        node=child
                                    />
                                }
                            })
                            .collect_view()}
                    </ul>
                </details>
            </li>
        }
        .into_any();
    }
    let size = size.unwrap_or_default();
    let label = view! {
        <span class="archive-meta">{human_size(size)}" · "{kind.clone()}</span>
    };
    let class = format!("archive-name kind-{kind}");
    view! {
        <li>
            {if kind == "archive" {
                let mut next_containers = containers;
                next_containers.push(path);
                let href = archive_listing_href(&route, &project, &sha256, &filename, &next_containers);
                view! { <a class=class href=href>{name}</a> }.into_any()
            } else if previewable {
                let href = archive_member_href(&route, &project, &sha256, &filename, &containers, &path, 0);
                view! { <a class=class href=href>{name}</a> }.into_any()
            } else {
                view! { <span class=class>{name}</span> }.into_any()
            }}
            {label}
        </li>
    }
    .into_any()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArchiveNode {
    name: String,
    path: String,
    size: Option<u64>,
    kind: String,
    previewable: bool,
    directory: bool,
    children: Vec<Self>,
}

#[derive(Default)]
struct ArchiveBranch {
    directories: std::collections::BTreeMap<String, Self>,
    files: Vec<UiMember>,
}

fn archive_tree(entries: Vec<UiMember>) -> Vec<ArchiveNode> {
    let mut root = ArchiveBranch::default();
    for entry in entries {
        root.insert(entry);
    }
    root.into_nodes("")
}

impl ArchiveBranch {
    fn insert(&mut self, entry: UiMember) {
        let parts = entry.path.split('/').map(str::to_owned).collect::<Vec<_>>();
        let mut branch = self;
        for directory in parts.iter().take(parts.len().saturating_sub(1)) {
            branch = branch.directories.entry(directory.clone()).or_default();
        }
        branch.files.push(entry);
    }

    fn into_nodes(self, prefix: &str) -> Vec<ArchiveNode> {
        self.directories
            .into_iter()
            .map(|(name, branch)| {
                let path = archive_child_path(prefix, &name);
                ArchiveNode {
                    name,
                    path: path.clone(),
                    size: None,
                    kind: "folder".to_owned(),
                    previewable: false,
                    directory: true,
                    children: branch.into_nodes(&path),
                }
            })
            .chain(self.files.into_iter().map(|file| {
                let name = file.path.rsplit('/').next().unwrap_or(&file.path).to_owned();
                ArchiveNode {
                    name,
                    path: file.path,
                    size: Some(file.size),
                    kind: file.kind,
                    previewable: file.previewable,
                    directory: false,
                    children: Vec::new(),
                }
            }))
            .collect()
    }
}

fn archive_child_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

#[component]
fn MemberView(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: String,
    offset: u64,
) -> impl IntoView {
    let content = Resource::new(
        {
            let key = (
                route.clone(),
                sha256.clone(),
                filename.clone(),
                containers.clone(),
                member.clone(),
                offset,
            );
            move || key.clone()
        },
        |(route, sha256, filename, containers, member, offset)| {
            load_member_chunk(route, sha256, filename, containers, member, offset)
        },
    );
    let back = archive_listing_href(&route, &project, &sha256, &filename, &containers);
    view! {
        <h1><code>{member.clone()}</code></h1>
        <p><a href=back>"back to archive"</a></p>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let project = project.clone();
                let sha256 = sha256.clone();
                let filename = filename.clone();
                let containers = containers.clone();
                let member = member.clone();
                Suspend::new(async move {
                    let chunk = content.await;
                    view! { <MemberChunk route project sha256 filename containers member chunk /> }
                })
            }}
        </Suspense>
    }
}

#[component]
fn MemberChunk(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: String,
    chunk: UiMemberChunk,
) -> impl IntoView {
    let next = chunk
        .next_offset
        .map(|offset| archive_member_href(&route, &project, &sha256, &filename, &containers, &member, offset));
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
    }
}

fn archive_listing_href(route: &str, project: &str, sha256: &str, filename: &str, containers: &[String]) -> String {
    let mut href = format!(
        "/browse?index={}&project={}&sha256={}&file={}",
        url_encode(route),
        url_encode(project),
        url_encode(sha256),
        url_encode(filename)
    );
    append_container_query(&mut href, containers);
    href
}

fn archive_member_href(
    route: &str,
    project: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
    member: &str,
    offset: u64,
) -> String {
    let mut href = archive_listing_href(route, project, sha256, filename, containers);
    href.push_str("&member=");
    href.push_str(&url_encode(member));
    if offset > 0 {
        href.push_str("&offset=");
        href.push_str(&offset.to_string());
    }
    href
}

fn append_container_query(href: &mut String, containers: &[String]) {
    for container in containers {
        href.push_str("&container=");
        href.push_str(&url_encode(container));
    }
}

#[component]
fn MetaPanel(doc: CoreMetadataDoc, versions: Vec<String>) -> impl IntoView {
    view! {
        <h3>"Versions"</h3>
        <p class="chips">{versions.into_iter().map(|version| view! { <code>{version}</code> }).collect_view()}</p>
        {doc.requires_python.map(|value| view! { <h3>"Requires Python"</h3><p><code>{value}</code></p> })}
        {doc.license.map(|value| view! { <h3>"License"</h3><p>{value}</p> })}
        {doc.author.map(|value| view! { <h3>"Author"</h3><p>{value}</p> })}
        {doc.maintainer.map(|value| view! { <h3>"Maintainer"</h3><p>{value}</p> })}
        {(!doc.keywords.is_empty()).then(|| view! {
            <h3>"Keywords"</h3>
            <p class="chips">{doc.keywords.into_iter().map(|word| view! { <code>{word}</code> }).collect_view()}</p>
        })}
        {(!doc.requires_dist.is_empty()).then(|| view! {
            <h3>"Dependencies"</h3>
            <p class="chips">
                {doc.requires_dist.into_iter().map(|dep| view! { <code>{dep}</code> }).collect_view()}
            </p>
        })}
        {(!doc.project_urls.is_empty()).then(|| view! {
            <h3>"Links"</h3>
            <ul class="links-list">
                {doc.project_urls
                    .into_iter()
                    .map(|(label, url)| view! { <li><a href=url rel="external">{label}</a></li> })
                    .collect_view()}
            </ul>
        })}
        <ClassifierGroups classifiers=doc.classifiers />
    }
}

/// Classifiers grouped by their top-level category, the way pypi.org presents them.
#[component]
fn ClassifierGroups(classifiers: Vec<String>) -> impl IntoView {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for classifier in classifiers {
        let (group, rest) = classifier.split_once(" :: ").map_or_else(
            || (classifier.clone(), classifier.clone()),
            |(g, r)| (g.to_owned(), r.to_owned()),
        );
        match groups.iter_mut().find(|(name, _)| *name == group) {
            Some((_, values)) => values.push(rest),
            None => groups.push((group, vec![rest])),
        }
    }
    (!groups.is_empty()).then(|| {
        view! {
            <h3>"Classifiers"</h3>
            {groups
                .into_iter()
                .map(|(group, values)| {
                    view! {
                        <p class="classifier-group">{group}</p>
                        <ul class="classifiers">
                            {values.into_iter().map(|value| view! { <li>{value}</li> }).collect_view()}
                        </ul>
                    }
                })
                .collect_view()}
        }
    })
}

/// Yank, un-yank, and delete for the index's local layer, driven from the browser with the upload
/// token. The buttons only act once hydrated; the server renders them inert.
#[component]
fn AdminPanel(
    route: String,
    project: String,
    versions: Vec<String>,
    refresh: Resource<Option<(UiProject, Option<CoreMetadataDoc>)>>,
    token: ReadSignal<String>,
    set_token: WriteSignal<String>,
    set_outcome: WriteSignal<String>,
) -> impl IntoView {
    let act = move |method: &'static str, url: String| {
        run_admin(method, url, token.get_untracked(), set_outcome, refresh);
    };
    let rows = versions
        .into_iter()
        .map(|version| {
            let yank_url = format!("/{route}/{project}/{version}/yank");
            let delete_url = format!("/{route}/{project}/{version}/");
            let unyank_url = yank_url.clone();
            view! {
                <tr>
                    <td><code>{version}</code></td>
                    <td>
                        <button on:click=move |_| act("PUT", yank_url.clone())>"yank"</button>
                        <button on:click=move |_| act("DELETE", unyank_url.clone())>"un-yank"</button>
                        <button class="danger" on:click=move |_| act("DELETE", delete_url.clone())>"delete"</button>
                    </td>
                </tr>
            }
        })
        .collect_view();
    let delete_all = format!("/{route}/{project}/");
    view! {
        <details class="admin">
            <summary>"Manage uploads"</summary>
            <p class="dim">"Actions apply to files uploaded to this index's local layer and need its upload token."</p>
            <input
                class="token"
                type="password"
                placeholder="upload token"
                on:input:target=move |event| set_token.set(event.target().value())
            />
            <table class="admin-table"><tbody>{rows}</tbody></table>
            <button class="danger" on:click=move |_| act("DELETE", delete_all.clone())>"delete whole project"</button>
        </details>
    }
}

fn run_admin(
    method: &'static str,
    url: String,
    token: String,
    outcome: WriteSignal<String>,
    refresh: Resource<Option<(UiProject, Option<CoreMetadataDoc>)>>,
) {
    #[cfg(feature = "hydrate")]
    {
        leptos::task::spawn_local(async move {
            let result = crate::data::admin_request(method, &url, &token).await;
            outcome.set(result);
            refresh.refetch();
        });
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (method, url, token, outcome, refresh);
    }
}

/// Render a byte count like pypi.org: one decimal in the largest fitting unit.
fn human_size(bytes: u64) -> String {
    #[allow(
        clippy::cast_precision_loss,
        reason = "display only; artifacts are far below 2^52 bytes"
    )]
    let mut value = bytes as f64;
    for unit in ["B", "kB", "MB", "GB"] {
        if value < 1024.0 {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.1} TB")
}

fn split_legacy_archive_file(file: &str) -> (String, String) {
    file.split_once('/')
        .map(|(sha256, filename)| (sha256.to_owned(), filename.to_owned()))
        .unwrap_or_default()
}

/// The usage statistics drill-down: every index, one index's projects, or one project's files,
/// selected by query parameters, with the aggregate of the current level on top.
#[component]
pub fn Stats() -> impl IntoView {
    let query = use_query_map();
    let route = Memo::new(move |_| query.read().get("index").filter(|name| !name.is_empty()));
    let project = Memo::new(move |_| query.read().get("project").filter(|name| !name.is_empty()));
    view! {
        <section class="page">
            {move || {
                let key = (route.get(), project.get());
                view! { <StatsView route=key.0 project=key.1 /> }
            }}
        </section>
    }
}

#[component]
fn StatsView(route: Option<String>, project: Option<String>) -> impl IntoView {
    let stats = Resource::new(
        {
            let key = (route.clone(), project.clone());
            move || key.clone()
        },
        |(route, project)| load_stats(route, project),
    );
    let crumb = match (&route, &project) {
        (Some(index), Some(name)) => view! {
            <p class="breadcrumb">
                <a href="/stats">"usage"</a>
                " / "
                <a href=format!("/stats?index={}", url_encode(index))>{index.clone()}</a>
                " / "
                <span>{name.clone()}</span>
            </p>
        }
        .into_any(),
        (Some(index), None) => view! {
            <p class="breadcrumb">
                <a href="/stats">"usage"</a>
                " / "
                <span>{index.clone()}</span>
            </p>
        }
        .into_any(),
        _ => view! { <p class="breadcrumb"><span>"usage"</span></p> }.into_any(),
    };
    view! {
        {crumb}
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let project = project.clone();
                Suspend::new(async move {
                    let data = stats.await;
                    view! { <StatsBody route project data /> }
                })
            }}
        </Suspense>
    }
}

#[component]
fn StatsBody(route: Option<String>, project: Option<String>, data: UiStats) -> impl IntoView {
    let totals = data.totals;
    let empty = data.rows.is_empty();
    let (label, rows) = match (&route, &project) {
        (Some(_), Some(_)) => ("File", file_rows(data.rows)),
        (Some(index), None) => (
            "Project",
            drill_rows(data.rows, |name| {
                format!("/stats?index={}&project={}", url_encode(index), url_encode(name))
            }),
        ),
        _ => (
            "Index",
            drill_rows(data.rows, |name| format!("/stats?index={}", url_encode(name))),
        ),
    };
    view! {
        <div class="stat-row">
            <div class="stat"><strong>{totals.pages}</strong><span>"pages"</span></div>
            <div class="stat"><strong>{totals.downloads}</strong><span>"downloads"</span></div>
            <div class="stat"><strong>{human_size(totals.bytes)}</strong><span>"served"</span></div>
            <div class="stat"><strong>{totals.metadata}</strong><span>"metadata hits"</span></div>
            <div class="stat"><strong>{totals.uploads}</strong><span>"uploads"</span></div>
            <div class="stat"><strong>{totals.refreshes}</strong><span>"refreshes"</span></div>
            <div class="stat"><strong>{totals.changed}</strong><span>"upstream changes"</span></div>
            <div class="stat"><strong>{totals.stale_served}</strong><span>"stale fallbacks"</span></div>
            <div class="stat"><strong>{totals.upstream_errors}</strong><span>"upstream errors"</span></div>
            <div class="stat"><strong>{totals.rejected}</strong><span>"rejected downloads"</span></div>
        </div>
        <table class="files stats-table">
            <thead>
                <tr>
                    <th>{label}</th><th>"Pages"</th><th>"Downloads"</th><th>"Served"</th>
                    <th>"Metadata"</th><th>"Uploads"</th>
                </tr>
            </thead>
            <tbody>{rows}</tbody>
        </table>
        {empty.then(|| view! { <p class="dim">"Nothing recorded at this level yet."</p> })}
    }
}

/// Rows whose names drill one level deeper.
fn drill_rows(rows: Vec<(String, UiCounters)>, href: impl Fn(&str) -> String) -> Vec<AnyView> {
    rows.into_iter()
        .map(|(name, c)| {
            let link = href(&name);
            view! {
                <tr>
                    <td><a href=link>{name}</a></td>
                    <td>{c.pages}</td>
                    <td>{c.downloads}</td>
                    <td>{human_size(c.bytes)}</td>
                    <td>{c.metadata}</td>
                    <td>{c.uploads}</td>
                </tr>
            }
            .into_any()
        })
        .collect()
}

/// Leaf rows: files have no deeper level to link to.
fn file_rows(rows: Vec<(String, UiCounters)>) -> Vec<AnyView> {
    rows.into_iter()
        .map(|(name, c)| {
            view! {
                <tr>
                    <td><code>{name}</code></td>
                    <td>{c.pages}</td>
                    <td>{c.downloads}</td>
                    <td>{human_size(c.bytes)}</td>
                    <td>{c.metadata}</td>
                    <td>{c.uploads}</td>
                </tr>
            }
            .into_any()
        })
        .collect()
}
