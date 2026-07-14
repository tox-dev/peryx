#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]
#![allow(
    clippy::missing_const_for_fn,
    reason = "cfg-split helpers are const only without the hydrate feature; constness cannot vary by cfg"
)]

use std::sync::Arc;

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use peryx_core::{UiBlock, UiMeta};
use regex::Regex;

use super::{ErrorMessage, copy_to_clipboard, human_size};
use crate::data::load_project_view;
use crate::markdown::{EXTERNAL_LINK_REL, is_safe_artifact_link, is_safe_link, render_description};
use crate::model::{UiFile, UiProject, UiProjectView};
#[cfg(feature = "hydrate")]
use crate::url::browser_http_origin;
use crate::url::{
    admin_project_url, admin_version_url, browse_archive_url, browse_index_url, browse_project_file_search_url,
    browse_ref_url, simple_index_url,
};

type ProjectPage = Result<Option<UiProjectView>, String>;
type ProjectPageResource = Resource<ProjectPage>;

#[component]
pub(super) fn ProjectView(route: String, project: String) -> impl IntoView {
    let page = Resource::new(
        {
            let key = (route.clone(), project.clone());
            move || key.clone()
        },
        |(route, project)| load_project_view(route, project),
    );
    // Admin state lives here, outside the Suspend scope: signals created inside async-hydrated
    // suspense content are disposed once hydration completes, which would make them inert.
    let (token, set_token) = signal(String::new());
    let (outcome, set_outcome) = signal(String::new());
    let crumb_project = project.clone();
    view! {
        <p class="breadcrumb">
            <a href=browse_index_url(&route)>{route.clone()}</a>
            " / "
            <span>{crumb_project}</span>
        </p>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let project = project.clone();
                Suspend::new(async move {
                    match page.await {
                        Ok(Some(UiProjectView::Files { project: ui, meta })) => {
                            view! { <ProjectBody route ui meta refresh=page token set_token set_outcome /> }
                                .into_any()
                        }
                        Ok(Some(UiProjectView::References { names })) => {
                            view! { <ReferenceList route project names /> }.into_any()
                        }
                        Ok(None) => view! { <p class="dim">"Project not found on this index."</p> }.into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                        // `UiProjectView` is `#[non_exhaustive]`: a browse shape this renderer does not
                        // yet know renders a notice rather than a blank page.
                        _ => view! { <p class="dim">"Unsupported project view."</p> }.into_any(),
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

/// A registry repository's references (tags), each linking to the manifest it resolves to.
#[component]
fn ReferenceList(route: String, project: String, names: Vec<String>) -> impl IntoView {
    let count = names.len();
    let empty = names.is_empty();
    let rows = names
        .into_iter()
        .map(|name| {
            let href = browse_ref_url(&route, &project, &name);
            view! { <tr><td><a href=href>{name}</a></td></tr> }
        })
        .collect_view();
    view! {
        <h1><code>{project}</code></h1>
        {empty.then(|| view! { <p class="dim">"No tags for this repository yet."</p> })}
        {(!empty)
            .then(|| view! {
                <p class="dim">{count}" tag(s)"</p>
                <div class="table-scroll">
                    <table class="files">
                        <thead><tr><th>"Tag"</th></tr></thead>
                        <tbody>{rows}</tbody>
                    </table>
                </div>
            })}
    }
}

#[component]
fn ProjectBody(
    route: String,
    ui: UiProject,
    meta: UiMeta,
    refresh: ProjectPageResource,
    token: ReadSignal<String>,
    set_token: WriteSignal<String>,
    set_outcome: WriteSignal<String>,
) -> impl IntoView {
    let latest = meta
        .version
        .clone()
        .or_else(|| ui.versions.last().cloned())
        .unwrap_or_default();
    let description = meta.description.as_ref().map(render_description).unwrap_or_default();
    let summary = meta.summary.clone();
    view! {
        <header class="project-head">
            <h1>{ui.name.clone()} <span class="version">{latest}</span></h1>
            {summary.map(|summary| view! { <p class="summary">{summary}</p> })}
            <InstallSnippet index_url=simple_index_url(&route) project=ui.name.clone() />
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
                <MetaPanel meta versions=ui.versions />
            </aside>
        </div>
    }
}

#[component]
fn InstallSnippet(index_url: String, project: String) -> impl IntoView {
    let (install, set_install) = signal(install_command("", &index_url, &project));
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            if let Some(location) = web_sys::window().map(|window| window.location())
                && let Ok(protocol) = location.protocol()
                && let Ok(hostname) = location.hostname()
                && let Ok(port) = location.port()
                && let Some(origin) = browser_http_origin(&protocol, &hostname, &port)
            {
                set_install.set(install_command(&origin, &index_url, &project));
            }
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = set_install;
    view! {
        <div class="install">
            <code>{move || install.get()}</code>
            <button class="copy" title="Copy" on:click=move |_| install.with_untracked(|command| copy_to_clipboard(command))>"copy"</button>
        </div>
    }
}

fn install_command(origin: &str, index_url: &str, project: &str) -> String {
    let mut command = String::with_capacity(origin.len() + index_url.len() + project.len() + 32);
    command.push_str("uv pip install --index-url ");
    command.push_str(origin);
    command.push_str(index_url);
    command.push(' ');
    command.push_str(project);
    command
}

#[component]
fn FileTable(route: String, project: String, files: Vec<UiFile>) -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let files = Arc::new(files);
    let initial = FileSearch::from_query(&query.read());
    let (initial_matches, initial_error) = match matching_file_indexes(&files, &initial.pattern, initial.mode) {
        Ok(indexes) => (indexes, None),
        Err(message) => ((0..files.len()).collect(), Some(message)),
    };
    let (pattern, set_pattern) = signal(initial.pattern);
    let (mode, set_mode) = signal(initial.mode);
    let (matches, set_matches) = signal(initial_matches);
    let (error, set_error) = signal(initial_error);
    let total = files.len();
    Effect::new(move |_| {
        let search = FileSearch::from_query(&query.read());
        if pattern.get_untracked() != search.pattern {
            set_pattern.set(search.pattern);
        }
        if mode.get_untracked() != search.mode {
            set_mode.set(search.mode);
        }
    });
    Effect::new({
        let files = files.clone();
        move |_| match matching_file_indexes(&files, &pattern.get(), mode.get()) {
            Ok(indexes) => {
                set_error.set(None);
                set_matches.set(indexes);
            }
            Err(message) => set_error.set(Some(message)),
        }
    });
    view! {
        <div class="file-filter">
            <input
                class="search file-search"
                type="search"
                placeholder="Filter filenames"
                prop:value=move || pattern.get()
                on:input:target={
                    let navigate = navigate.clone();
                    let route = route.clone();
                    let project = project.clone();
                    move |event| {
                        let next = event.target().value();
                        let replace = pattern.get_untracked().is_empty() == next.is_empty();
                        set_pattern.set(next.clone());
                        navigate(
                            &browse_project_file_search_url(
                                &route,
                                &project,
                                &next,
                                mode.get_untracked() == FileSearchMode::Regex,
                            ),
                            NavigateOptions { replace, scroll: false, ..NavigateOptions::default() },
                        );
                    }
                }
            />
            <label class="file-filter-mode">
                <input
                    type="checkbox"
                    prop:checked=move || mode.get() == FileSearchMode::Regex
                    on:change:target={
                        let navigate = navigate.clone();
                        let route = route.clone();
                        let project = project.clone();
                        move |event| {
                            let next = if event.target().checked() {
                                FileSearchMode::Regex
                            } else {
                                FileSearchMode::Substring
                            };
                            set_mode.set(next);
                            navigate(
                                &browse_project_file_search_url(
                                    &route,
                                    &project,
                                    &pattern.get_untracked(),
                                    next == FileSearchMode::Regex,
                                ),
                                NavigateOptions { replace: false, scroll: false, ..NavigateOptions::default() },
                            );
                        }
                    }
                />
                <span>"regex"</span>
            </label>
            <span class="file-filter-count">
                {move || file_count(matches.with(Vec::len), total)}
            </span>
        </div>
        {move || error.get().map(|message| view! { <p class="error">{message}</p> })}
        <table class="files">
            <thead><tr><th>"File"</th><th>"Size"</th><th>"Uploaded"</th><th>"sha256"</th><th>"Flags"</th></tr></thead>
            <tbody>
                {move || {
                    if matches.with(Vec::is_empty) {
                        view! { <tr><td colspan="5" class="empty">"No artifacts match this filename filter."</td></tr> }
                            .into_any()
                    } else {
                        matches
                            .get()
                            .into_iter()
                            .map(|index| file_row(&route, &project, &files[index]))
                            .collect_view()
                            .into_any()
                    }
                }}
            </tbody>
        </table>
    }
}

fn file_row(route: &str, project: &str, file: &UiFile) -> impl IntoView {
    let class = if file.yanked { "yanked" } else { "" };
    let filename = file.filename.clone();
    let download = if is_safe_artifact_link(&file.url) {
        view! { <a href=file.url.clone()>{filename}</a> }.into_any()
    } else {
        filename.into_any()
    };
    let inspect = browse_archive_url(route, project, &file.sha256, &file.filename);
    let short_hash = file.sha256.get(..12).unwrap_or_default().to_owned();
    view! {
        <tr class=class>
            <td>
                {download}
                {supports_archive_browser(&file.filename)
                    .then(|| view! {
                        " · "
                        <a class="inspect" href=inspect>"contents"</a>
                    })}
            </td>
            <td>{file.size.map_or_else(|| "—".to_owned(), human_size)}</td>
            <td>{file.upload_time.clone().map_or_else(|| "—".to_owned(), |time| time.chars().take(10).collect())}</td>
            <td><code title=file.sha256.clone()>{short_hash}</code></td>
            <td>
                {file.yanked.then(|| view! { <span class="badge yanked-badge">"yanked"</span> })}
                {file.has_metadata.then(|| view! { <span class="badge meta-badge">"metadata"</span> })}
            </td>
        </tr>
    }
}

fn file_count(shown: usize, total: usize) -> String {
    if shown == total {
        format!("{total} {}", file_label(total))
    } else {
        format!("{shown} of {total} {}", file_label(total))
    }
}

fn file_label(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSearch {
    pattern: String,
    mode: FileSearchMode,
}

impl FileSearch {
    fn from_query(query: &leptos_router::params::ParamsMap) -> Self {
        Self {
            pattern: query.get("filename").unwrap_or_default(),
            mode: FileSearchMode::from_query(query.get_str("filename_match")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileSearchMode {
    Substring,
    Regex,
}

impl FileSearchMode {
    fn from_query(value: Option<&str>) -> Self {
        match value {
            Some("regex") => Self::Regex,
            _ => Self::Substring,
        }
    }
}

fn matching_file_indexes(files: &[UiFile], pattern: &str, mode: FileSearchMode) -> Result<Vec<usize>, String> {
    if pattern.is_empty() {
        return Ok((0..files.len()).collect());
    }
    match mode {
        FileSearchMode::Substring => {
            let needle = pattern.to_lowercase();
            Ok(files
                .iter()
                .enumerate()
                .filter_map(|(index, file)| file.filename.to_lowercase().contains(&needle).then_some(index))
                .collect())
        }
        FileSearchMode::Regex => {
            let regex = Regex::new(pattern).map_err(|err| format!("Invalid regex: {err}"))?;
            Ok(files
                .iter()
                .enumerate()
                .filter_map(|(index, file)| regex.is_match(&file.filename).then_some(index))
                .collect())
        }
    }
}

fn supports_archive_browser(filename: &str) -> bool {
    let path = std::path::Path::new(filename);
    path.extension().is_some_and(|ext| {
        ext.eq_ignore_ascii_case("whl")
            || ext.eq_ignore_ascii_case("zip")
            || ext.eq_ignore_ascii_case("egg")
            || ext.eq_ignore_ascii_case("tar")
    }) || filename
        .get(filename.len().saturating_sub(7)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
        || filename
            .get(filename.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tgz"))
}

#[component]
fn MetaPanel(meta: UiMeta, versions: Vec<String>) -> impl IntoView {
    let blocks = meta.blocks.into_iter().map(block_view).collect_view();
    view! {
        <h3>"Versions"</h3>
        <p class="chips">{versions.into_iter().map(|version| view! { <code>{version}</code> }).collect_view()}</p>
        {blocks}
    }
}

/// Render one neutral metadata block. The catch-all keeps an unrecognized block — a variant added to
/// [`UiBlock`] that this renderer does not yet know — from breaking the page.
fn block_view(block: UiBlock) -> AnyView {
    match block {
        UiBlock::KeyValue { label, value } => view! {
            <h3>{label}</h3><p><code>{value}</code></p>
        }
        .into_any(),
        UiBlock::Chips { label, values } => view! {
            <h3>{label}</h3>
            <p class="chips">{values.into_iter().map(|value| view! { <code>{value}</code> }).collect_view()}</p>
        }
        .into_any(),
        UiBlock::Links { label, links } => view! {
            <h3>{label}</h3>
            <ul class="links-list">
                {links.into_iter().map(|(text, url)| {
                    if is_safe_link(&url) {
                        view! { <li><a href=url rel=EXTERNAL_LINK_REL>{text}</a></li> }.into_any()
                    } else {
                        view! { <li>{text}</li> }.into_any()
                    }
                }).collect_view()}
            </ul>
        }
        .into_any(),
        UiBlock::Groups { label, groups } => view! {
            <h3>{label}</h3>
            {groups.into_iter().map(|(group, values)| view! {
                <p class="classifier-group">{group}</p>
                <ul class="classifiers">
                    {values.into_iter().map(|value| view! { <li>{value}</li> }).collect_view()}
                </ul>
            }).collect_view()}
        }
        .into_any(),
        _ => ().into_any(),
    }
}

/// Yank, un-yank, and delete for the index's hosted layer, driven from the browser with the upload
/// token. The buttons only act once hydrated; the server renders them inert.
#[component]
fn AdminPanel(
    route: String,
    project: String,
    versions: Vec<String>,
    refresh: ProjectPageResource,
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
            let yank_url = admin_version_url(&route, &project, &version, Some("yank"));
            let delete_url = admin_version_url(&route, &project, &version, None);
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
    let delete_all = admin_project_url(&route, &project);
    view! {
        <details class="admin">
            <summary>"Manage uploads"</summary>
            <p class="dim">"Actions apply to files uploaded to this index's hosted layer and need its upload token."</p>
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
    refresh: ProjectPageResource,
) {
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        leptos::task::spawn_local(async move {
            let result = crate::data::admin_request(method, &url, &token).await;
            outcome.set(result);
            refresh.refetch();
        });
    }
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    {
        let _ = (method, url, token, outcome, refresh);
    }
}
