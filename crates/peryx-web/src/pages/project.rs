#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]
#![allow(
    clippy::missing_const_for_fn,
    reason = "cfg-split helpers are const only without the hydrate feature; constness cannot vary by cfg"
)]

use std::collections::HashMap;
use std::sync::Arc;

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use peryx_core::{UiBlock, UiMeta};
use regex::Regex;

use super::{ErrorMessage, copy_to_clipboard, human_size};
use crate::data::load_project_view;
use crate::markdown::{external_link_rel, is_safe_artifact_link, is_safe_link};
use crate::model::{
    UiArtifactSource, UiAttestation, UiByteAvailability, UiFile, UiProject, UiProjectStatus, UiProjectView,
    UiProvenance, UiProvenanceSource, UiRelease, byte_availability_label, file_source_label, provenance_source_label,
    provenance_validation_label, subject_match_label,
};
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use crate::url::browser_http_origin;
use crate::url::{
    admin_project_url, admin_version_url, browse_archive_url, browse_index_url, browse_project_file_search_url,
    browse_project_release_url, browse_ref_url,
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
    let UiProject {
        name,
        status,
        versions,
        files,
        client_command,
    } = ui;
    let latest = meta
        .version
        .clone()
        .or_else(|| versions.last().map(|release| release.version.clone()))
        .unwrap_or_default();
    let description = meta.description.clone().unwrap_or_default();
    let notice = description.notice;
    let summary = meta.summary.clone();
    let admin_versions = versions.iter().map(|release| release.version.clone()).collect();
    view! {
        <header class="project-head">
            <h1>
                {name.clone()} <span class="version">{latest}</span>
                {status.map(|status| project_status_badge(*status))}
            </h1>
            {summary.map(|summary| view! { <p class="summary">{summary}</p> })}
            {client_command.map(|template| view! { <ClientCommand template /> })}
        </header>
        <div class="project-grid">
            <div class="project-main">
                <h2>"Description"</h2>
                {notice.map(|notice| view! { <p class="dim">{notice}</p> })}
                <div class="description" inner_html=description.html></div>
                <h2>"Files"</h2>
                <FileTable
                    route=route.clone()
                    project=name.clone()
                    releases=versions.clone()
                    files
                />
                <AdminPanel route=route.clone() project=name.clone() versions=admin_versions refresh token set_token set_outcome />
            </div>
            <aside class="project-side">
                <MetaPanel route project=name meta releases=versions />
            </aside>
        </div>
    }
}

/// The status badge beside the project heading: the marker keyed to its style, and the publisher's
/// reason. The reason comes from the package, so it renders as text, never as markup.
fn project_status_badge(status: UiProjectStatus) -> impl IntoView {
    let UiProjectStatus { marker, reason } = status;
    let class = format!("badge status-{marker}");
    view! {
        <span class=class>{marker}</span>
        {reason.map(|reason| view! { <span class="status-reason">{reason}</span> })}
    }
}

#[component]
fn ClientCommand(template: String) -> impl IntoView {
    let (command, set_command) = signal(template.replace("<origin>", ""));
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        Effect::new(move |_| {
            if let Some(location) = web_sys::window().map(|window| window.location())
                && let Ok(protocol) = location.protocol()
                && let Ok(hostname) = location.hostname()
                && let Ok(port) = location.port()
                && let Some(origin) = browser_http_origin(&protocol, &hostname, &port)
            {
                set_command.set(template.replace("<origin>", &origin));
            }
        });
    }
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    let _ = set_command;
    view! {
        <div class="install">
            <code>{move || command.get()}</code>
            <button class="copy" title="Copy" on:click=move |_| command.with_untracked(|value| copy_to_clipboard(value))>"copy"</button>
        </div>
    }
}

#[component]
fn FileTable(route: String, project: String, releases: Vec<UiRelease>, files: Vec<UiFile>) -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let files = Arc::new(files);
    let groups = Arc::new(group_file_indexes(&releases, &files));
    let initial = FileSearch::from_query(&query.read());
    let (initial_matches, initial_error) = match matching_files(&files, &initial.pattern, initial.mode) {
        Ok(matches) => (matches, None),
        Err(message) => (vec![true; files.len()], Some(message)),
    };
    let (pattern, set_pattern) = signal(initial.pattern);
    let (mode, set_mode) = signal(initial.mode);
    let (version, set_version) = signal(initial.version);
    let (matches, set_matches) = signal(initial_matches);
    let (error, set_error) = signal(initial_error);
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        Effect::new(move |_| {
            let search = FileSearch::from_query(&query.read());
            if pattern.get_untracked() != search.pattern {
                set_pattern.set(search.pattern);
            }
            if mode.get_untracked() != search.mode {
                set_mode.set(search.mode);
            }
            if version.get_untracked() != search.version {
                set_version.set(search.version);
            }
        });
        Effect::new({
            let files = files.clone();
            move |_| match matching_files(&files, &pattern.get(), mode.get()) {
                Ok(next_matches) => {
                    set_error.set(None);
                    set_matches.set(next_matches);
                }
                Err(message) => set_error.set(Some(message)),
            }
        });
    }
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    let _ = (set_matches, set_error, set_version);
    let count_groups = groups.clone();
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
                                version.get_untracked().as_deref(),
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
                                    version.get_untracked().as_deref(),
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
                {move || {
                    let selected = version.get();
                    let (shown, total) = group_file_count(&count_groups, &matches.read(), selected.as_deref());
                    file_count(shown, total)
                }}
            </span>
        </div>
        {move || error.get().map(|message| view! { <p class="error">{message}</p> })}
        {move || {
            file_groups_view(
                &route,
                &project,
                &files,
                &groups,
                &matches.read(),
                version.get().as_deref(),
                !pattern.get().is_empty(),
            )
        }}
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileGroup {
    release: Option<UiRelease>,
    indexes: Vec<usize>,
}

/// The adapter resolves version parsing and ambiguity; this layer follows explicit release labels.
fn group_file_indexes(releases: &[UiRelease], files: &[UiFile]) -> Vec<FileGroup> {
    let mut groups: Vec<FileGroup> = releases
        .iter()
        .cloned()
        .map(|release| FileGroup {
            release: Some(release),
            indexes: Vec::new(),
        })
        .collect();
    let mut group_by_version = HashMap::with_capacity(releases.len());
    for (index, release) in releases.iter().enumerate() {
        group_by_version.entry(release.version.as_str()).or_insert(index);
    }
    let mut legacy = Vec::new();
    for (index, file) in files.iter().enumerate() {
        match file
            .release
            .as_deref()
            .and_then(|version| group_by_version.get(version))
        {
            Some(&group) => groups[group].indexes.push(index),
            None => legacy.push(index),
        }
    }
    if !legacy.is_empty() {
        groups.push(FileGroup {
            release: None,
            indexes: legacy,
        });
    }
    groups
}

fn group_file_count(groups: &[FileGroup], matches: &[bool], selected: Option<&str>) -> (usize, usize) {
    let groups = groups
        .iter()
        .filter(|group| selected.is_none_or(|selected| group.release.as_ref().is_some_and(|r| r.version == selected)));
    groups.fold((0, 0), |(shown, total), group| {
        (
            shown + group.indexes.iter().filter(|&&index| matches[index]).count(),
            total + group.indexes.len(),
        )
    })
}

fn file_groups_view(
    route: &str,
    project: &str,
    files: &[UiFile],
    groups: &[FileGroup],
    matches: &[bool],
    selected: Option<&str>,
    filtering: bool,
) -> AnyView {
    if let Some(selected) = selected {
        return groups
            .iter()
            .enumerate()
            .find(|(_, group)| {
                group
                    .release
                    .as_ref()
                    .is_some_and(|release| release.version == selected)
            })
            .map_or_else(
                || {
                    view! {
                        <p class="dim release-empty">
                            "Release " <code>{selected.to_owned()}</code> " is not listed for this project."
                        </p>
                    }
                    .into_any()
                },
                |(position, group)| file_group_view(route, project, files, group, matches, position, filtering),
            );
    }
    if groups.is_empty() {
        return view! { <p class="dim release-empty">"No releases or files are available."</p> }.into_any();
    }
    if filtering && group_file_count(groups, matches, None).0 == 0 {
        return view! { <p class="dim release-empty">"No artifacts match this filename filter."</p> }.into_any();
    }
    groups
        .iter()
        .enumerate()
        .filter(|(_, group)| !filtering || group.indexes.iter().any(|&index| matches[index]))
        .map(|(position, group)| file_group_view(route, project, files, group, matches, position, filtering))
        .collect_view()
        .into_any()
}

fn file_group_view(
    route: &str,
    project: &str,
    files: &[UiFile],
    group: &FileGroup,
    matches: &[bool],
    position: usize,
    filtering: bool,
) -> AnyView {
    let (heading_id, heading, yanked, reasons) = group.release.as_ref().map_or_else(
        || {
            (
                "legacy-files".to_owned(),
                "Legacy or unassociated files".to_owned(),
                false,
                Vec::new(),
            )
        },
        |release| {
            (
                format!("release-{position}"),
                format!("Release {}", release.version),
                release.yanked,
                release.yanked_reasons.clone(),
            )
        },
    );
    let indexes: Vec<usize> = group.indexes.iter().copied().filter(|&index| matches[index]).collect();
    let empty = if group.indexes.is_empty() {
        Some("No files are associated with this release.")
    } else if indexes.is_empty() && filtering {
        Some("No artifacts match this filename filter.")
    } else {
        None
    };
    let legacy = group.release.is_none();
    let heading_element_id = heading_id.clone();
    view! {
        <section class="release-files" aria-labelledby=heading_id>
            <h3 id=heading_element_id>
                {heading}
                {yanked.then(|| view! { <span class="badge yanked-badge">"yanked"</span> })}
            </h3>
            {(!reasons.is_empty()).then(|| view! {
                <ul class="yank-reasons">
                    {reasons.into_iter().map(|reason| view! { <li>{reason}</li> }).collect_view()}
                </ul>
            })}
            {legacy.then(|| view! {
                <p class="dim release-note">
                    "These files do not match one declared release, so peryx keeps them separate."
                </p>
            })}
            <div class="table-scroll">
                <table class="files">
                    <thead><tr><th>"File"</th><th>"Size"</th><th>"Uploaded"</th><th>"sha256"</th><th>"Flags"</th></tr></thead>
                    <tbody>
                        {empty.map_or_else(
                            || indexes.into_iter().map(|index| file_row(route, project, &files[index])).collect_view().into_any(),
                            |message| view! { <tr><td colspan="5" class="empty">{message}</td></tr> }.into_any(),
                        )}
                    </tbody>
                </table>
            </div>
        </section>
    }
    .into_any()
}

fn file_row(route: &str, project: &str, file: &UiFile) -> impl IntoView {
    let class = if file.yanked { "yanked" } else { "" };
    let filename = file.filename.clone();
    let download = if is_safe_artifact_link(&file.url) {
        let rel = external_link_rel(&file.url);
        view! { <a href=file.url.clone() rel=rel>{filename}</a> }.into_any()
    } else {
        filename.into_any()
    };
    let inspect = (file.browsable && is_sha256_hex(&file.sha256))
        .then(|| browse_archive_url(route, project, &file.sha256, &file.filename));
    let provenance = provenance_panel(file);
    let short_hash = file.sha256.get(..12).unwrap_or_default().to_owned();
    view! {
        <tr class=class>
            <td>
                {download}
                {inspect.map(|href| view! {
                    " · "
                    <a class="inspect" href=href>"contents"</a>
                })}
                {provenance}
            </td>
            <td>{file.size.map_or_else(|| "-".to_owned(), human_size)}</td>
            <td>{file.upload_time.clone().map_or_else(|| "-".to_owned(), |time| time.chars().take(10).collect())}</td>
            <td><code title=file.sha256.clone()>{short_hash}</code></td>
            <td>
                {placement_badges(file.source, file.availability)}
                {file.yanked.then(|| view! { <span class="badge yanked-badge">"yanked"</span> })}
                {file.yanked_reason.clone().map(|reason| view! { <span class="yank-reason">{reason}</span> })}
                {file.has_metadata.then(|| view! { <span class="badge meta-badge">"metadata"</span> })}
                {file.upstream.clone().map(|upstream| view! {
                    <span class="badge" title="Upstream source">{upstream}</span>
                })}
            </td>
        </tr>
    }
}

/// The provenance panel for one file, when it advertises provenance. A keyboard-operable
/// `<details>` states the source and validation result in its summary and, when expanded, lists each
/// attestation's predicate type and subject binding. Everything is plain, escaped data derived from
/// stored metadata: peryx never verifies a Sigstore signature, and the full document is reachable
/// only through the download route the summary links, never inlined here.
fn provenance_panel(file: &UiFile) -> Option<AnyView> {
    let detail = file.provenance_detail.clone()?;
    let source = provenance_source_label(detail.source);
    let validation = provenance_validation_label(detail.source, detail.malformed);
    // The document link vets the scheme first, so a hostile advertised URL is dropped without hiding
    // the panel; the file, and the panel's summary, stay listed regardless.
    let document = file
        .provenance
        .clone()
        .filter(|url| is_safe_artifact_link(url))
        .map(|url| {
            let rel = external_link_rel(&url);
            view! { <a class="provenance-doc" href=url rel=rel>"provenance document"</a> }
        });
    let body = provenance_body(&detail);
    Some(
        view! {
            <details class="provenance-panel">
                <summary>
                    <span class="provenance-tag">"provenance"</span>
                    <span
                        class=format!("badge prov-source src-{}", source.key)
                        title=source.hint
                        aria-label=format!("provenance source: {}", source.text)
                    >{source.text}</span>
                    <span
                        class=format!("badge prov-validation val-{}", validation.key)
                        title=validation.hint
                        aria-label=format!("provenance validation: {}", validation.text)
                    >{validation.text}</span>
                </summary>
                <div class="provenance-body">
                    {body}
                    {document}
                </div>
            </details>
        }
        .into_any(),
    )
}

/// The expanded provenance body: the per-attestation list for a readable hosted document, or a note
/// for a mirrored claim or an unreadable document.
fn provenance_body(detail: &UiProvenance) -> AnyView {
    if detail.malformed {
        return view! {
            <p class="dim">"peryx could not read the stored provenance document for this file."</p>
        }
        .into_any();
    }
    if detail.source == UiProvenanceSource::Mirrored {
        return view! {
            <p class="dim">"An upstream index advertises provenance for this file. peryx relays the claim and has neither fetched nor verified it."</p>
        }
        .into_any();
    }
    // A hosted panel reaches here only with a summarized document, which always carries at least one
    // attestation: `serving::web` reports an empty or unreadable document as `malformed` above.
    let count = detail.attestations.len();
    let rows = detail.attestations.iter().cloned().map(attestation_row).collect_view();
    view! {
        <p class="dim">{format!("{count} {}", attestation_label(count))}</p>
        <ul class="attestations">{rows}</ul>
    }
    .into_any()
}

/// One attestation row: its declared in-toto predicate type and how its subject binds this file. The
/// predicate type comes from the bundle, so it renders as escaped text, never as markup.
fn attestation_row(attestation: UiAttestation) -> impl IntoView {
    let subject = subject_match_label(attestation.subject);
    let predicate = attestation.predicate_type.map_or_else(
        || view! { <span class="dim predicate-type">"no predicate type"</span> }.into_any(),
        |value| view! { <code class="predicate-type">{value}</code> }.into_any(),
    );
    view! {
        <li class="attestation">
            {predicate}
            <span
                class=format!("badge subject-{}", subject.key)
                title=subject.hint
                aria-label=format!("subject binding: {}", subject.text)
            >{subject.text}</span>
        </li>
    }
}

fn attestation_label(count: usize) -> &'static str {
    if count == 1 { "attestation" } else { "attestations" }
}

/// The two #441 placement chips a file carries: its source and its byte availability. Each names its
/// dimension for assistive tech and states its value in words, so the pair stays legible without
/// colour and composes with, rather than replaces, the yank, metadata, and upstream flags beside it.
fn placement_badges(source: UiArtifactSource, availability: UiByteAvailability) -> impl IntoView {
    let src = file_source_label(source);
    let avail = byte_availability_label(availability);
    view! {
        <span
            class=format!("badge placement-source src-{}", src.key)
            title=src.hint
            aria-label=format!("source: {}", src.text)
        >{src.text}</span>
        <span
            class=format!("badge placement-avail avail-{}", avail.key)
            title=avail.hint
            aria-label=format!("availability: {}", avail.text)
        >{avail.text}</span>
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
    version: Option<String>,
}

impl FileSearch {
    fn from_query(query: &leptos_router::params::ParamsMap) -> Self {
        Self {
            pattern: query.get("filename").unwrap_or_default(),
            mode: FileSearchMode::from_query(query.get_str("filename_match")),
            version: query.get("version"),
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

fn matching_files(files: &[UiFile], pattern: &str, mode: FileSearchMode) -> Result<Vec<bool>, String> {
    if pattern.is_empty() {
        return Ok(vec![true; files.len()]);
    }
    match mode {
        FileSearchMode::Substring => {
            let needle = pattern.to_lowercase();
            Ok(files
                .iter()
                .map(|file| file.filename.to_lowercase().contains(&needle))
                .collect())
        }
        FileSearchMode::Regex => {
            let regex = Regex::new(pattern).map_err(|err| format!("Invalid regex: {err}"))?;
            Ok(files.iter().map(|file| regex.is_match(&file.filename)).collect())
        }
    }
}

/// The archive route addresses an artifact by digest, so a Simple API file that omits its sha256 has
/// nothing to browse.
fn is_sha256_hex(sha256: &str) -> bool {
    sha256.len() == 64
        && sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[component]
fn MetaPanel(route: String, project: String, meta: UiMeta, releases: Vec<UiRelease>) -> impl IntoView {
    let query = use_query_map();
    let blocks = meta.blocks.into_iter().map(block_view).collect_view();
    view! {
        <h3>"Versions"</h3>
        <nav aria-label="Project releases">
            <ul class="releases">
                <li class="release">
                    <a
                        class="release-link"
                        href={
                            let route = route.clone();
                            let project = project.clone();
                            move || {
                                let search = FileSearch::from_query(&query.read());
                                browse_project_file_search_url(
                                    &route,
                                    &project,
                                    None,
                                    &search.pattern,
                                    search.mode == FileSearchMode::Regex,
                                )
                            }
                        }
                        aria-current=move || query.read().get_str("version").is_none().then_some("page")
                    >
                        "All releases"
                    </a>
                </li>
                {releases
                    .into_iter()
                    .map(|release| view! {
                        <ReleaseRow route=route.clone() project=project.clone() release />
                    })
                    .collect_view()}
            </ul>
        </nav>
        {blocks}
    }
}

/// One release: its version, a badge when its publisher yanked the whole release, and the reasons
/// they gave. The reasons come from the package, so they render as text, never as markup.
#[component]
fn ReleaseRow(route: String, project: String, release: UiRelease) -> impl IntoView {
    let query = use_query_map();
    let UiRelease {
        version,
        yanked,
        yanked_reasons,
    } = release;
    let link_version = version.clone();
    let current_version = version.clone();
    let reasons = (!yanked_reasons.is_empty()).then(|| {
        view! {
            <ul class="yank-reasons">
                {yanked_reasons.into_iter().map(|reason| view! { <li>{reason}</li> }).collect_view()}
            </ul>
        }
    });
    view! {
        <li class=if yanked { "release yanked" } else { "release" }>
            <a
                class="release-link"
                href=move || {
                    let search = FileSearch::from_query(&query.read());
                    browse_project_release_url(
                        &route,
                        &project,
                        &link_version,
                        &search.pattern,
                        search.mode == FileSearchMode::Regex,
                    )
                }
                aria-current=move || {
                    (query.read().get_str("version") == Some(current_version.as_str())).then_some("page")
                }
            >
                <code>{version}</code>
            </a>
            {yanked.then(|| view! { <span class="badge yanked-badge">"yanked"</span> })}
            {reasons}
        </li>
    }
}

/// Render one neutral metadata block. The catch-all keeps an unrecognized block - a variant added to
/// [`UiBlock`] that this renderer does not yet know - from breaking the page.
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
                        let rel = external_link_rel(&url);
                        view! { <li><a href=url rel=rel>{text}</a></li> }.into_any()
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

#[cfg(test)]
#[path = "../../tests/unit/pages/project/tests.rs"]
mod tests;
