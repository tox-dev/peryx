//! Producing the web UI's neutral view models from the `PyPI` serving layer, so the web crate renders
//! a project page without any knowledge of the Simple API, wheels, or PEP 658.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_core::{
    BrowseBadge, BrowseCell, BrowseLink, BrowsePage, BrowseProperty, BrowseRow, BrowseSection, UiAction,
    UiActionMethod, UiArtifactSource, UiByteAvailability,
};
use peryx_driver::access::ReadAccess;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{BrowseDriver as _, BrowseError, BrowseRequest};
use peryx_driver::{AppState, ServingState};
use peryx_ha::{ArtifactPlacement, ArtifactPlacementStore, ArtifactSource, ByteAvailability};
use peryx_identity::{Denial, ResourceMatch};
use peryx_index::{Index, IndexKind};
use peryx_storage::blob::{BlobLease, Digest};

use crate::cache::{self, CacheError};
use crate::store::PypiStore as _;
use crate::view::{
    AttestationView, FileView, MetadataBlock, MetadataView, ProjectView, ProvenanceSource, ProvenanceView, SubjectMatch,
};
use crate::{
    ProjectDetail, file_matches_version, normalize_name, normalize_name_cow, parse_version, to_json, ui_meta,
    ui_project_from_detail,
};

pub(super) const BROWSE_PATHS: &[&str] = &[
    "/upload",
    "/+ui/projects",
    "/+ui/project",
    "/+ui/members",
    "/+ui/member",
    "/+ui/browse",
];

pub(super) async fn browse_http(state: Arc<AppState>, request: Request) -> Response {
    if request.uri().path() == "/upload" {
        return if request.method() == Method::GET {
            super::upload_ui::response(&state)
        } else {
            StatusCode::METHOD_NOT_ALLOWED.into_response()
        };
    }
    let raw_query = request.uri().query().unwrap_or_default().to_owned();
    let query = match BrowseQuery::parse(&raw_query) {
        Ok(query) => query,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    let Some(position) = state
        .serving
        .indexes
        .iter()
        .position(|index| index.route == query.index && index.ecosystem == crate::ECOSYSTEM)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let access = ReadAccess::for_request(&state.serving, request.headers());
    let base = BaseUrl::from_request(
        request.headers(),
        request.uri(),
        request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|ConnectInfo(address)| state.serving.rate_limits.trusts_proxy(address.ip())),
    );
    match super::PypiServing
        .browse(BrowseRequest {
            state: state.serving.clone(),
            position,
            raw_query,
            access: &access,
            base: base.as_ref(),
        })
        .await
    {
        Ok(Some(page)) => no_store(Json(page).into_response()),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(BrowseError::Denied(denial)) => denial_response(denial),
        Err(BrowseError::Refused { content_type, body }) => {
            (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, content_type)], body).into_response()
        }
        Err(BrowseError::Internal(message)) => (StatusCode::INTERNAL_SERVER_ERROR, message).into_response(),
    }
}

pub(super) async fn browse(
    state: Arc<ServingState>,
    position: usize,
    raw_query: &str,
    access: &ReadAccess,
) -> Result<Option<BrowsePage>, BrowseError> {
    let query = BrowseQuery::parse(raw_query)?;
    let index_access = access.for_index(state.index_at(position));
    // An ACL resource pattern names a PEP 503 normalized project, as upload and mutation authorize;
    // the raw spelling stays for the display name the store resolves.
    query.project.as_deref().map_or_else(
        || index_access.authorize_any_resource(),
        |project| index_access.authorize_resource(ResourceMatch::Pattern(&normalize_name_cow(project))),
    )?;
    if let Some(project) = query.project.clone() {
        if let Some((digest, filename)) = query.archive()? {
            return archive_page(state, position, project, digest, filename, query)
                .await
                .map(Some);
        }
        return project_page(state, position, project)
            .await?
            .map(|(project, metadata)| project_browse_page(&query, project, metadata))
            .transpose()
            .map_err(BrowseError::from);
    }
    Ok(Some(project_list_page(
        &state.index_at(position).route,
        project_names(&state, position)?,
    )))
}

#[derive(Clone, Default)]
struct BrowseQuery {
    index: String,
    project: Option<String>,
    version: Option<String>,
    filename: Option<String>,
    filename_match: Option<String>,
    sha256: Option<String>,
    file: Option<String>,
    containers: Vec<String>,
    member: Option<String>,
    offset: u64,
}

impl BrowseQuery {
    fn parse(raw: &str) -> Result<Self, String> {
        let mut query = Self::default();
        for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            let value = value.into_owned();
            match key.as_ref() {
                "index" => query.index = value,
                "project" if !value.is_empty() => query.project = Some(value),
                "version" if !value.is_empty() => query.version = Some(value),
                "filename" if !value.is_empty() => query.filename = Some(value),
                "filename_match" if !value.is_empty() => query.filename_match = Some(value),
                "sha256" if !value.is_empty() => query.sha256 = Some(value),
                "file" if !value.is_empty() => query.file = Some(value),
                "container" if !value.is_empty() => query.containers.push(value),
                "member" if !value.is_empty() => query.member = Some(value),
                "offset" if !value.is_empty() => {
                    query.offset = value.parse().map_err(|_| format!("invalid archive offset {value:?}"))?;
                }
                _ => {}
            }
        }
        if query.index.is_empty() {
            return Err("missing index".to_owned());
        }
        Ok(query)
    }

    fn archive(&self) -> Result<Option<(String, String)>, String> {
        match (&self.sha256, &self.file) {
            (Some(digest), Some(filename)) => Ok(Some((digest.clone(), filename.clone()))),
            (None, Some(address)) => address
                .split_once('/')
                .map(|(digest, filename)| Some((digest.to_owned(), filename.to_owned())))
                .ok_or_else(|| "archive query requires sha256".to_owned()),
            (Some(_), None) => Err("archive query requires file".to_owned()),
            (None, None) if self.member.is_some() || !self.containers.is_empty() => {
                Err("archive query requires file and sha256".to_owned())
            }
            (None, None) => Ok(None),
        }
    }
}

fn denial_response(denial: Denial) -> Response {
    match denial {
        Denial::Unauthenticated => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, peryx_driver::route_auth::BASIC_CHALLENGE)],
            "unauthorized",
        )
            .into_response(),
        Denial::Forbidden | Denial::Unavailable => StatusCode::FORBIDDEN.into_response(),
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn project_list_page(route: &str, names: Vec<String>) -> BrowsePage {
    BrowsePage {
        title: route.to_owned(),
        sections: vec![BrowseSection::Links {
            heading: "Projects".to_owned(),
            entries: names
                .into_iter()
                .map(|name| BrowseLink {
                    href: browse_url(route, Some(&name), &[]),
                    label: name,
                })
                .collect(),
            empty: "No projects observed on this index yet.".to_owned(),
        }],
        ..BrowsePage::default()
    }
}

fn project_browse_page(
    query: &BrowseQuery,
    project: ProjectView,
    metadata: MetadataView,
) -> Result<BrowsePage, String> {
    let file_regex = match (query.filename.as_deref(), query.filename_match.as_deref()) {
        (Some(pattern), Some("regex")) => {
            Some(regex::Regex::new(pattern).map_err(|error| format!("invalid regex: {error}"))?)
        }
        _ => None,
    };
    let mut sections = metadata_sections(metadata.description, metadata.blocks);
    sections.push(release_section(&query.index, &project));
    sections.push(file_section(query, &project, file_regex.as_ref()));
    Ok(BrowsePage {
        breadcrumbs: vec![BrowseLink {
            label: query.index.clone(),
            href: browse_url(&query.index, None, &[]),
        }],
        title: project.name,
        subtitle: metadata.version,
        summary: metadata.summary,
        command: project.client_command,
        badges: project
            .status
            .into_iter()
            .map(|status| BrowseBadge {
                class: format!("status-{}", status.marker),
                label: status.marker,
                hint: status.reason,
            })
            .collect(),
        sections,
        actions: project.actions,
    })
}

fn metadata_sections(
    description: Option<peryx_core::RenderedDescription>,
    blocks: Vec<MetadataBlock>,
) -> Vec<BrowseSection> {
    let mut sections = description
        .map(|description| BrowseSection::Markup {
            heading: "Description".to_owned(),
            html: description.html,
            notice: description.notice,
        })
        .into_iter()
        .collect::<Vec<_>>();
    sections.extend(blocks.into_iter().map(|block| {
        match block {
            MetadataBlock::KeyValue { label, value } => BrowseSection::Properties {
                heading: label.clone(),
                entries: vec![BrowseProperty {
                    label,
                    value,
                    href: None,
                }],
            },
            MetadataBlock::Chips { label, values } => BrowseSection::Properties {
                heading: label.clone(),
                entries: values
                    .into_iter()
                    .map(|value| BrowseProperty {
                        label: label.clone(),
                        value,
                        href: None,
                    })
                    .collect(),
            },
            MetadataBlock::Links { label, links } => BrowseSection::Links {
                heading: label,
                entries: links
                    .into_iter()
                    .map(|(label, href)| BrowseLink { label, href })
                    .collect(),
                empty: String::new(),
            },
            MetadataBlock::Groups { label, groups } => BrowseSection::Properties {
                heading: label,
                entries: groups
                    .into_iter()
                    .map(|(label, values)| BrowseProperty {
                        label,
                        value: values.join(", "),
                        href: None,
                    })
                    .collect(),
            },
        }
    }));
    sections
}

fn release_section(route: &str, project: &ProjectView) -> BrowseSection {
    BrowseSection::Table {
        heading: "Releases".to_owned(),
        columns: vec!["Version".to_owned(), "Status".to_owned()],
        rows: project
            .versions
            .iter()
            .map(|release| BrowseRow {
                cells: vec![
                    BrowseCell {
                        text: release.version.clone(),
                        href: Some(browse_url(
                            route,
                            Some(&project.name),
                            &[("version", release.version.as_str())],
                        )),
                        code: true,
                    },
                    BrowseCell {
                        text: release
                            .lifecycle
                            .as_ref()
                            .map_or_else(|| "active".to_owned(), |lifecycle| lifecycle.label.clone()),
                        href: None,
                        code: false,
                    },
                ],
                badges: lifecycle_badges(release.lifecycle.as_ref()),
                actions: release.actions.clone(),
            })
            .collect(),
        empty: "No releases are listed.".to_owned(),
    }
}

fn file_section(query: &BrowseQuery, project: &ProjectView, regex: Option<&regex::Regex>) -> BrowseSection {
    BrowseSection::Table {
        heading: "Files".to_owned(),
        columns: ["File", "Version", "Size", "Uploaded", "Source", "Availability"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        rows: project
            .files
            .iter()
            .filter(|file| {
                query
                    .version
                    .as_ref()
                    .is_none_or(|version| file.release.as_ref() == Some(version))
            })
            .filter(|file| filename_matches(file, query.filename.as_deref(), regex))
            .map(|file| file_row(query, project, file))
            .collect(),
        empty: "No files match this query.".to_owned(),
    }
}

fn filename_matches(file: &FileView, pattern: Option<&str>, regex: Option<&regex::Regex>) -> bool {
    regex.map_or_else(
        || pattern.is_none_or(|pattern| file.filename.to_lowercase().contains(&pattern.to_lowercase())),
        |regex| regex.is_match(&file.filename),
    )
}

fn file_row(query: &BrowseQuery, project: &ProjectView, file: &FileView) -> BrowseRow {
    BrowseRow {
        cells: vec![
            BrowseCell {
                text: file.filename.clone(),
                href: file.browsable.then(|| {
                    browse_url(
                        &query.index,
                        Some(&project.name),
                        &[("sha256", file.sha256.as_str()), ("file", file.filename.as_str())],
                    )
                }),
                code: true,
            },
            BrowseCell {
                text: file.release.clone().unwrap_or_default(),
                href: None,
                code: true,
            },
            BrowseCell {
                text: file.size.map_or_else(String::new, |size| size.to_string()),
                href: None,
                code: false,
            },
            BrowseCell {
                text: file.upload_time.clone().unwrap_or_default(),
                href: None,
                code: false,
            },
            BrowseCell {
                text: file.source.as_str().to_owned(),
                href: file.upstream.clone(),
                code: false,
            },
            BrowseCell {
                text: file.availability.as_str().to_owned(),
                href: None,
                code: false,
            },
        ],
        badges: lifecycle_badges(file.lifecycle.as_ref())
            .into_iter()
            .chain(provenance_badges(file.provenance_detail.as_ref()))
            .collect(),
        actions: Vec::new(),
    }
}

fn lifecycle_badges(lifecycle: Option<&crate::view::LifecycleView>) -> Vec<BrowseBadge> {
    lifecycle
        .map(|lifecycle| BrowseBadge {
            label: lifecycle.label.clone(),
            class: format!("lifecycle-{}", lifecycle.label),
            hint: (!lifecycle.reasons.is_empty()).then(|| lifecycle.reasons.join("; ")),
        })
        .into_iter()
        .collect()
}

fn provenance_badges(provenance: Option<&ProvenanceView>) -> impl Iterator<Item = BrowseBadge> + '_ {
    provenance.into_iter().map(|provenance| BrowseBadge {
        label: match provenance.source {
            ProvenanceSource::Hosted => "hosted provenance",
            ProvenanceSource::Mirrored => "upstream provenance",
        }
        .to_owned(),
        class: if provenance.malformed {
            "provenance-malformed"
        } else {
            "provenance-valid"
        }
        .to_owned(),
        hint: attestation_hint(&provenance.attestations),
    })
}

fn attestation_hint(attestations: &[AttestationView]) -> Option<String> {
    (!attestations.is_empty()).then(|| {
        attestations
            .iter()
            .map(|attestation| {
                let subject = match attestation.subject {
                    SubjectMatch::Matched => "matched",
                    SubjectMatch::Mismatched => "mismatched",
                    SubjectMatch::Unknown => "unknown",
                };
                attestation
                    .predicate_type
                    .as_ref()
                    .map_or_else(|| subject.to_owned(), |predicate| format!("{predicate}: {subject}"))
            })
            .collect::<Vec<_>>()
            .join("; ")
    })
}

async fn archive_page(
    state: Arc<ServingState>,
    position: usize,
    project: String,
    digest: String,
    filename: String,
    query: BrowseQuery,
) -> Result<BrowsePage, BrowseError> {
    let lease = artifact_path_in_project(state, position, project.clone(), digest.clone(), filename.clone()).await?;
    match query.member.clone() {
        Some(member) => archive_member_page(query, project, digest, filename, member, lease).await,
        None => archive_listing_page(query, project, digest, filename, lease).await,
    }
    .map_err(BrowseError::from)
}

async fn archive_listing_page(
    query: BrowseQuery,
    project: String,
    digest: String,
    filename: String,
    lease: BlobLease,
) -> Result<BrowsePage, String> {
    let task_filename = filename.clone();
    let containers = query.containers.clone();
    let members = tokio::task::spawn_blocking(move || {
        crate::archive::list_members_nested_path(&task_filename, lease.path(), &containers)
    })
    .await
    .map_err(super::archive_listing_task_error)?
    .map_err(crate::error_message)?;
    Ok(BrowsePage {
        breadcrumbs: archive_breadcrumbs(&query.index, &project, &digest, &filename, &query.containers),
        title: query.containers.last().cloned().unwrap_or_else(|| filename.clone()),
        sections: vec![BrowseSection::Table {
            heading: "Archive members".to_owned(),
            columns: vec!["Path".to_owned(), "Size".to_owned(), "Kind".to_owned()],
            rows: members
                .into_iter()
                .map(|member| {
                    let href = if member.kind.as_str() == "archive" {
                        Some(archive_url(
                            &query,
                            &project,
                            &digest,
                            &filename,
                            Some((&member.path, true)),
                            None,
                        ))
                    } else if member.previewable {
                        Some(archive_url(
                            &query,
                            &project,
                            &digest,
                            &filename,
                            Some((&member.path, false)),
                            None,
                        ))
                    } else {
                        None
                    };
                    BrowseRow {
                        cells: vec![
                            BrowseCell {
                                text: member.path,
                                href,
                                code: true,
                            },
                            BrowseCell {
                                text: member.size.to_string(),
                                href: None,
                                code: false,
                            },
                            BrowseCell {
                                text: member.kind.as_str().to_owned(),
                                href: None,
                                code: false,
                            },
                        ],
                        badges: Vec::new(),
                        actions: Vec::new(),
                    }
                })
                .collect(),
            empty: "The archive has no members.".to_owned(),
        }],
        ..BrowsePage::default()
    })
}

async fn archive_member_page(
    query: BrowseQuery,
    project: String,
    digest: String,
    filename: String,
    member: String,
    lease: BlobLease,
) -> Result<BrowsePage, String> {
    let task_filename = filename.clone();
    let containers = query.containers.clone();
    let selected = member.clone();
    let chunk = tokio::task::spawn_blocking(move || {
        crate::archive::read_text_member_chunk_nested_path(
            &task_filename,
            lease.path(),
            &containers,
            &selected,
            query.offset,
            crate::archive::DEFAULT_MEMBER_CHUNK,
        )
    })
    .await
    .map_err(super::archive_member_task_error)?
    .map_err(crate::error_message)?;
    Ok(BrowsePage {
        breadcrumbs: archive_breadcrumbs(&query.index, &project, &digest, &filename, &query.containers),
        title: member.clone(),
        sections: vec![BrowseSection::Content {
            heading: "Member".to_owned(),
            text: super::member_text(&member, chunk.bytes)?,
            size: Some(chunk.size),
            offset: chunk.offset,
            next: chunk.next_offset.map(|offset| BrowseLink {
                label: "Next chunk".to_owned(),
                href: archive_url(
                    &query,
                    &project,
                    &digest,
                    &filename,
                    Some((&member, false)),
                    Some(offset),
                ),
            }),
        }],
        ..BrowsePage::default()
    })
}

fn archive_breadcrumbs(
    route: &str,
    project: &str,
    digest: &str,
    filename: &str,
    containers: &[String],
) -> Vec<BrowseLink> {
    let mut links = vec![
        BrowseLink {
            label: route.to_owned(),
            href: browse_url(route, None, &[]),
        },
        BrowseLink {
            label: project.to_owned(),
            href: browse_url(route, Some(project), &[]),
        },
        BrowseLink {
            label: filename.to_owned(),
            href: browse_url(route, Some(project), &[("sha256", digest), ("file", filename)]),
        },
    ];
    for count in 0..containers.len() {
        let mut serializer = browse_serializer(route, Some(project));
        serializer.append_pair("sha256", digest).append_pair("file", filename);
        for container in &containers[..=count] {
            serializer.append_pair("container", container);
        }
        links.push(BrowseLink {
            label: containers[count].clone(),
            href: serializer.finish(),
        });
    }
    links
}

fn archive_url(
    query: &BrowseQuery,
    project: &str,
    digest: &str,
    filename: &str,
    selected: Option<(&str, bool)>,
    offset: Option<u64>,
) -> String {
    let mut serializer = browse_serializer(&query.index, Some(project));
    serializer.append_pair("sha256", digest).append_pair("file", filename);
    for container in &query.containers {
        serializer.append_pair("container", container);
    }
    if let Some((path, container)) = selected {
        serializer.append_pair(if container { "container" } else { "member" }, path);
    }
    if let Some(offset) = offset {
        serializer.append_pair("offset", &offset.to_string());
    }
    serializer.finish()
}

fn browse_url(route: &str, project: Option<&str>, pairs: &[(&str, &str)]) -> String {
    let mut serializer = browse_serializer(route, project);
    serializer.extend_pairs(pairs.iter().copied());
    serializer.finish()
}

fn browse_serializer(route: &str, project: Option<&str>) -> url::form_urlencoded::Serializer<'static, String> {
    let mut serializer = url::form_urlencoded::Serializer::for_suffix("/browse?".to_owned(), "/browse?".len());
    serializer.append_pair("index", route);
    if let Some(project) = project {
        serializer.append_pair("project", project);
    }
    serializer
}

#[cfg(test)]
#[path = "../../tests/unit/serving/web/url_tests.rs"]
mod url_tests;

#[cfg(test)]
#[path = "../../tests/unit/serving/web/placement_tests.rs"]
mod placement_tests;

pub(super) fn project_names(state: &ServingState, position: usize) -> Result<Vec<String>, String> {
    let list = cache::resolve_list(state, state.index_at(position))?;
    Ok(list.projects.into_iter().map(|entry| entry.name).collect())
}

/// A project's page data: its files as a neutral [`UiProject`], and the neutral [`UiMeta`] the
/// page's default release carries in a PEP 658 metadata sibling.
pub(super) async fn project_page(
    state: Arc<ServingState>,
    position: usize,
    project: String,
) -> Result<Option<(ProjectView, MetadataView)>, String> {
    let route = state.index_at(position).route.clone();
    let normalized = normalize_name(&project);
    let index = state.index_at(position);
    let Some((detail, hosted)) = resolve_detail_and_hosted(&state, index, &normalized, &route)
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
    // `to_json` serializes the detail, so parsing it straight back cannot fail.
    let value = serde_json::from_str(&to_json(&detail)).expect("to_json emits JSON that round-trips");
    let mut ui = ui_project_from_detail(&value);
    if let Some(display) = state
        .meta
        .get_project(&index.name, &normalized)
        .map_err(crate::error_message)?
    {
        ui.name = display;
    }
    apply_actions(&route, &mut ui);
    apply_placement(&state, &index.name, &normalized, &hosted, &mut ui).map_err(crate::error_message)?;
    apply_provenance(&state, &hosted, &normalized, &mut ui).await;
    let default = default_version(&ui);
    // A pre-PEP 700 upstream names no versions, so no release owns a file and the newest sibling stands in.
    let sibling = match default.as_deref() {
        Some(version) => metadata_file(&ui, version),
        None => ui.files.iter().rev().find(|file| file.has_metadata),
    };
    // The view model carries the digest as text for the UI, so the sibling is only reachable once
    // that text parses back into the content address the blob store is keyed by.
    let addressed = sibling.and_then(|file| Digest::from_hex(&file.sha256).map(|digest| (file, digest)));
    let mut meta = match addressed {
        Some((file, digest)) => metadata_for(&state, index, &route, file, digest).await?,
        None => MetadataView::default(),
    };
    meta.version = default.or(meta.version);
    ui.client_command = Some(install_command(&route, &ui.name, meta.version.as_deref()));
    Ok(Some((ui, meta)))
}

fn apply_actions(route: &str, project: &mut ProjectView) {
    project.actions.push(UiAction {
        label: "delete whole project".to_owned(),
        method: UiActionMethod::Delete,
        endpoint: mutation_endpoint(route, &project.name, None, None),
        destructive: true,
    });
    for release in &mut project.versions {
        let endpoint = mutation_endpoint(route, &project.name, Some(&release.version), Some("yank"));
        release.actions = vec![
            UiAction {
                label: "yank".to_owned(),
                method: UiActionMethod::Put,
                endpoint: endpoint.clone(),
                destructive: false,
            },
            UiAction {
                label: "un-yank".to_owned(),
                method: UiActionMethod::Delete,
                endpoint,
                destructive: false,
            },
            UiAction {
                label: "delete".to_owned(),
                method: UiActionMethod::Delete,
                endpoint: mutation_endpoint(route, &project.name, Some(&release.version), None),
                destructive: true,
            },
        ];
    }
}

fn mutation_endpoint(route: &str, project: &str, version: Option<&str>, action: Option<&str>) -> String {
    let mut endpoint = String::new();
    endpoint.push('/');
    peryx_core::url_encoding::push_path(&mut endpoint, route);
    endpoint.push('/');
    peryx_core::url_encoding::push_component(&mut endpoint, project);
    endpoint.push('/');
    if let Some(version) = version {
        peryx_core::url_encoding::push_component(&mut endpoint, version);
        endpoint.push('/');
    }
    if let Some(action) = action {
        endpoint.push_str(action);
    }
    endpoint
}

fn install_command(route: &str, project: &str, version: Option<&str>) -> String {
    let target = version.map_or_else(
        || shell_quote(project),
        |version| shell_quote(&format!("{project}=={version}")),
    );
    let mut endpoint = String::new();
    endpoint.push_str("<origin>/");
    peryx_core::url_encoding::push_path(&mut endpoint, route);
    endpoint.push_str("/simple/");
    format!("uv pip install --index-url {endpoint} {target}")
}

fn shell_quote(value: &str) -> String {
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'=' | b'+' | b':' | b'@' | b'/' | b',')
    }) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Resolve a project's detail together with the filenames its hosted layers published, so both store
/// reads share the caller's one error mapping.
async fn resolve_detail_and_hosted(
    state: &ServingState,
    index: &Index,
    project: &str,
    route: &str,
) -> Result<Option<(ProjectDetail, BTreeMap<String, String>)>, CacheError> {
    let Some(detail) = cache::resolve_detail(state, index, project, route).await? else {
        return Ok(None);
    };
    let mut hosted = BTreeMap::new();
    collect_hosted_filenames(state, index, project, &mut hosted)?;
    Ok(Some((detail, hosted)))
}

/// The hosted (upload) layer that published each filename of `project`, merged across a virtual
/// index's layers so a merged page can tell an uploaded file from a mirrored one and can name the
/// layer that owns the publication.
///
/// The walk follows shadow order, the order [`cache::resolve_detail`] merges pages in, and keeps the
/// first layer to claim a filename, so the panel reads the same publication the page serves.
fn collect_hosted_filenames(
    state: &ServingState,
    index: &Index,
    project: &str,
    names: &mut BTreeMap<String, String>,
) -> Result<(), peryx_storage::meta::MetaError> {
    match &index.kind {
        IndexKind::Hosted { .. } => {
            for (filename, _record) in state.meta.list_upload_entries(&index.name, project)? {
                names.entry(filename).or_insert_with(|| index.name.clone());
            }
        }
        IndexKind::Virtual { layers, .. } => {
            for pos in peryx_index::shadow_order(&state.indexes, layers) {
                collect_hosted_filenames(state, state.index_at(pos), project, names)?;
            }
        }
        IndexKind::Cached { .. } => {}
    }
    Ok(())
}

/// Resolve each file's #441 placement (its source and its projected byte availability) from one
/// indexed lookup per file, without probing the content store. A hosted upload shadows a same-named
/// upstream file, the dependency-confusion order [`cache::resolve_detail`] merged the page by, so
/// hosted-layer membership forces the `Hosted` source over any stale proxied placement.
///
/// The availability comes straight from the stored projection, which a repair pass keeps in step with
/// the content store; a listing therefore never reads a blob per row. A file the placement store has
/// not recorded - an upstream catalog entry never fetched - stays proxied and remote-only.
fn apply_placement(
    state: &ServingState,
    index: &str,
    normalized: &str,
    hosted: &BTreeMap<String, String>,
    ui: &mut ProjectView,
) -> Result<(), peryx_storage::meta::MetaError> {
    for file in &mut ui.files {
        let is_hosted = hosted.contains_key(&file.filename);
        file.upstream = if is_hosted {
            None
        } else {
            state
                .meta
                .get_file_url(index, normalized, &file.sha256)?
                .and_then(|source| source.upstream)
        };
        let placement = ArtifactPlacementStore::get_artifact_placement(&state.meta, &file.sha256)?;
        let resolved = resolve_file_placement(is_hosted, placement);
        file.source = ui_source(resolved.source);
        file.availability = ui_availability(resolved.availability);
    }
    Ok(())
}

/// The source and availability to report for one file, given whether an upload record already
/// establishes that this node wrote its bytes.
///
/// The projection is consulted to demote that answer, never to establish it, because an absent row is
/// no observation at all: see [`ArtifactPlacement`]. A hosted upload therefore reads local until a
/// hosted-source row says its bytes went away, and a proxied file reads remote until a row says they
/// arrived. The upload path records no row for a still-present file today, so the hosted branch runs on
/// the upload record alone for most files; [#2141](https://github.com/tox-dev/peryx/issues/2141) is
/// where that gap closes, and closing it changes nothing here.
pub fn resolve_file_placement(hosted: bool, placement: Option<ArtifactPlacement>) -> ArtifactPlacement {
    if hosted {
        // A row left by a same-digest mirror describes that mirror, not this upload, so only a
        // hosted-source row demotes it.
        return match placement {
            Some(placement) if matches!(placement.source, ArtifactSource::Hosted) => placement,
            _ => ArtifactPlacement {
                source: ArtifactSource::Hosted,
                availability: ByteAvailability::Local,
            },
        };
    }
    placement.unwrap_or(ArtifactPlacement {
        source: ArtifactSource::Proxy,
        availability: ByteAvailability::RemoteOnly,
    })
}

const fn ui_source(source: ArtifactSource) -> UiArtifactSource {
    match source {
        ArtifactSource::Hosted => UiArtifactSource::Hosted,
        ArtifactSource::Proxy => UiArtifactSource::Proxy,
        ArtifactSource::Generated => UiArtifactSource::Generated,
    }
}

const fn ui_availability(availability: ByteAvailability) -> UiByteAvailability {
    match availability {
        ByteAvailability::Local => UiByteAvailability::Local,
        ByteAvailability::RemoteOnly => UiByteAvailability::RemoteOnly,
        ByteAvailability::Unavailable => UiByteAvailability::Unavailable,
    }
}

/// The largest stored provenance document the panel summarizer reads. A distribution's provenance
/// object stays well under this, which bounds the read regardless.
const MAX_PROVENANCE_BYTES: u64 = 2 * 1024 * 1024;

/// Fill each file's provenance panel from the publication's own record, read locally.
///
/// A hosted file's stored provenance document is summarized into per-attestation records; a mirrored
/// file is flagged as an upstream claim without reading or fetching its document. This reads only
/// local storage - it never calls upstream and never verifies a signature - so a listing stays a
/// projection of what peryx already holds.
async fn apply_provenance(
    state: &Arc<ServingState>,
    hosted: &BTreeMap<String, String>,
    normalized: &str,
    ui: &mut ProjectView,
) {
    for file in &mut ui.files {
        file.provenance_detail = provenance_detail(state, hosted, normalized, file).await;
    }
}

/// The provenance panel for one file, or `None` when it advertises no provenance. A hosted document
/// that cannot be read is reported as `malformed` rather than hidden, so the page never implies an
/// advertised attestation is absent.
async fn provenance_detail(
    state: &Arc<ServingState>,
    hosted: &BTreeMap<String, String>,
    normalized: &str,
    file: &FileView,
) -> Option<ProvenanceView> {
    file.provenance.as_ref()?;
    // `apply_placement` marks exactly the files in `hosted` as hosted, so membership is the source.
    let Some(index) = hosted.get(&file.filename) else {
        return Some(ProvenanceView {
            source: ProvenanceSource::Mirrored,
            attestations: Vec::new(),
            malformed: false,
        });
    };
    let attestations = hosted_attestations(state, index, normalized, file).await;
    Some(ProvenanceView {
        source: ProvenanceSource::Hosted,
        malformed: attestations.is_none(),
        attestations: attestations.unwrap_or_default(),
    })
}

/// The bundle the hosted layer that owns this publication accepted - not whatever bundle another
/// index published over the same bytes.
async fn hosted_attestations(
    state: &Arc<ServingState>,
    index: &str,
    normalized: &str,
    file: &FileView,
) -> Option<Vec<AttestationView>> {
    let (provenance_hex, _size) = state
        .meta
        .get_provenance(index, normalized, &file.sha256, &file.filename)
        .ok()??;
    let digest = Digest::from_hex(&provenance_hex)?;
    let bytes = state.blobs.read_bytes(&digest, MAX_PROVENANCE_BYTES).await.ok()?;
    crate::attestation::summarize_provenance(&bytes, &file.sha256, &file.filename)
}

/// The release the project page defaults to. An active release (one the publisher has not yanked
/// whole) outranks a yanked one, a stable release outranks a pre-release, and the greatest PEP 440
/// version wins within a class, the order the file-yanking specification and Warehouse use.
///
/// A version that does not parse as PEP 440 counts as neither stable nor greater than a parseable
/// one, so it wins only when nothing else can, and then the greatest string takes it.
fn default_version(project: &ProjectView) -> Option<String> {
    project
        .versions
        .iter()
        .map(|release| {
            let parsed = parse_version(&release.version);
            let stable = parsed.as_ref().is_some_and(|parsed| !parsed.any_prerelease());
            ((release.lifecycle.is_none(), stable, parsed), &release.version)
        })
        .max()
        .map(|(_, version)| version.clone())
}

/// The file whose PEP 658 metadata sibling describes `version`, so the page never borrows another
/// release's metadata. An active file outranks a yanked one, and the filename settles the rest, so a
/// release with several siblings always renders the same one.
fn metadata_file<'a>(project: &'a ProjectView, version: &str) -> Option<&'a FileView> {
    project
        .files
        .iter()
        .filter(|file| file.has_metadata && file_matches_version(&file.filename, version))
        .min_by(|left, right| {
            (left.lifecycle.is_some(), &left.filename).cmp(&(right.lifecycle.is_some(), &right.filename))
        })
}

/// Fetch and parse the PEP 658 metadata sibling of `file` into the neutral view model.
async fn metadata_for(
    state: &Arc<ServingState>,
    index: &Index,
    route: &str,
    file: &FileView,
    digest: Digest,
) -> Result<MetadataView, String> {
    let metadata_filename = format!("{}.metadata", file.filename);
    let bytes = cache::metadata_bytes(state, index, &digest, route, &metadata_filename)
        .await
        .map_err(|err| {
            format!(
                "metadata fetch on index {route:?} for file {:?} with digest {}: {}",
                file.filename,
                digest.as_str(),
                err.user_message()
            )
        })?;
    ui_meta(&String::from_utf8_lossy(&bytes))
        .map_err(|err| format!("metadata parse on index {route:?} for file {:?}: {err}", file.filename))
}

/// The local blob-store path of the artifact `digest_hex`/`filename` on the index at `position`,
/// fetching it through the proxy on a miss.
///
/// The download gate decides whether the artifact may be released at all, so the browser refuses a
/// quarantined or policy-denied artifact exactly as the file route does, and refuses a digest this
/// index does not publish with the same not-found the file route writes.
///
/// The one thing the shared gate cannot judge is the query's `project`, which is what the caller's
/// read was authorized against: the gate keys membership on the project the *filename* names, so a
/// browse of a project the caller may read must not reach a file naming another. Both refusals read
/// alike, so neither reports on what the index holds.
pub(super) async fn artifact_path_in_project(
    state: Arc<ServingState>,
    position: usize,
    project: String,
    digest_hex: String,
    filename: String,
) -> Result<BlobLease, BrowseError> {
    let index = state.index_at(position);
    let route = index.route.clone();
    let Some(digest) = Digest::from_hex(&digest_hex) else {
        return Err(BrowseError::Internal(format!(
            "artifact on index {route:?} for file {filename:?}: invalid sha256 digest {digest_hex:?}"
        )));
    };
    let context = |err: cache::CacheError| {
        BrowseError::Internal(format!(
            "artifact on index {route:?} for file {filename:?} with digest {digest_hex}: {}",
            err.user_message()
        ))
    };
    if normalize_name(&project) != crate::project_of_filename(&filename) {
        return Err(context(cache::CacheError::FileNotFound));
    }
    if let Some(refusal) = super::get::download_refusal(&state, index, &filename, &digest)
        .await
        .map_err(&context)?
    {
        return Err(BrowseError::Refused {
            content_type: refusal.content_type.to_owned(),
            body: refusal.body,
        });
    }
    let owner = index.name.clone();
    cache::file_path(state, owner, digest, route.clone(), filename.clone())
        .await
        .map_err(&context)
}
