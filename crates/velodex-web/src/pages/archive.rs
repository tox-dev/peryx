#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use super::{ErrorMessage, human_size};
use crate::data::{load_member_chunk, load_members};
use crate::model::{UiMember, UiMemberChunk};
use crate::url::{browse_archive_listing_url, browse_archive_member_url, browse_index_url, browse_project_url};

/// The archive browser: member listing of one distribution, or one member's content.
#[component]
pub(super) fn ArchiveView(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: Option<String>,
    offset: u64,
) -> impl IntoView {
    let back = browse_project_url(&route, &project);
    view! {
        <p class="breadcrumb">
            <a href=browse_index_url(&route)>{route.clone()}</a>
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
    let root = browse_archive_listing_url(&route, &project, &sha256, &filename, &[]);
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
                let href = browse_archive_listing_url(&route, &project, &sha256, &filename, &prefix);
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
                    match members.await {
                        Ok(entries) => view! { <ArchiveTree route project sha256 filename containers entries /> }
                            .into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
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
                let href = browse_archive_listing_url(&route, &project, &sha256, &filename, &next_containers);
                view! { <a class=class href=href>{name}</a> }.into_any()
            } else if previewable {
                let href = browse_archive_member_url(&route, &project, &sha256, &filename, &containers, &path, 0);
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
    let back = browse_archive_listing_url(&route, &project, &sha256, &filename, &containers);
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
                    match content.await {
                        Ok(chunk) => view! { <MemberChunk route project sha256 filename containers member chunk /> }
                            .into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
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
        .map(|offset| browse_archive_member_url(&route, &project, &sha256, &filename, &containers, &member, offset));
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

pub(super) fn split_legacy_archive_file(file: &str) -> (String, String) {
    file.split_once('/')
        .map(|(sha256, filename)| (sha256.to_owned(), filename.to_owned()))
        .unwrap_or_default()
}
