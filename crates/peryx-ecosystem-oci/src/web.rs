use peryx_core::{BrowseCell, BrowseLink, BrowsePage, BrowseProperty, BrowseRow, BrowseSection};
use serde::Serialize;

use crate::name::Reference;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestContentReference {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub browsable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestContent {
    pub media_type: String,
    pub is_index: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<ManifestContentReference>,
    pub entries: Vec<ManifestContentReference>,
    pub total_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    pub path: String,
    pub size: u64,
    pub kind: String,
    pub previewable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct MemberChunk {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositoryContent {
    References { names: Vec<String> },
}

#[must_use]
pub fn pull_command(name: &str, reference: &Reference) -> String {
    match reference {
        Reference::Tag(tag) => format!("docker pull <host>/{name}:{tag}"),
        Reference::Digest(digest) => format!("docker pull <host>/{name}@{digest}"),
    }
}

/// # Errors
/// Returns a message when the manifest is not valid JSON.
pub fn manifest_content_from_bytes(bytes: &[u8]) -> Result<ManifestContent, String> {
    serde_json::from_slice(bytes)
        .map(|value| manifest_content_from_json(&value))
        .map_err(|error| error.to_string())
}

#[must_use]
pub fn index_page(route: &str, repositories: Vec<String>) -> BrowsePage {
    BrowsePage {
        title: route.to_owned(),
        sections: vec![BrowseSection::Links {
            heading: "Repositories".to_owned(),
            entries: repositories
                .into_iter()
                .map(|repository| BrowseLink {
                    href: browse_url(route, &[("project", &repository)]),
                    label: repository,
                })
                .collect(),
            empty: "No repositories observed on this index yet.".to_owned(),
        }],
        ..BrowsePage::default()
    }
}

#[must_use]
pub fn repository_page(route: &str, repository: &str, references: Vec<String>) -> BrowsePage {
    BrowsePage {
        breadcrumbs: vec![BrowseLink {
            label: route.to_owned(),
            href: browse_url(route, &[]),
        }],
        title: repository.to_owned(),
        sections: vec![BrowseSection::Links {
            heading: "Tags".to_owned(),
            entries: references
                .into_iter()
                .map(|reference| BrowseLink {
                    href: browse_url(route, &[("project", repository), ("ref", &reference)]),
                    label: reference,
                })
                .collect(),
            empty: "No tags observed for this repository yet.".to_owned(),
        }],
        ..BrowsePage::default()
    }
}

#[must_use]
pub fn manifest_page(route: &str, repository: &str, reference: &str, manifest: ManifestContent) -> BrowsePage {
    let heading = if manifest.is_index {
        "Platform manifests"
    } else {
        "Layers"
    };
    let columns = if manifest.is_index {
        vec!["Digest", "Platform", "Size", "Media type"]
    } else {
        vec!["Digest", "Size", "Media type", "Contents"]
    };
    let rows = manifest
        .entries
        .into_iter()
        .map(|entry| manifest_row(route, repository, reference, manifest.is_index, entry))
        .collect();
    let mut properties = vec![
        BrowseProperty {
            label: "Media type".to_owned(),
            value: manifest.media_type,
            href: None,
        },
        BrowseProperty {
            label: "Total size".to_owned(),
            value: manifest.total_size.to_string(),
            href: None,
        },
    ];
    if let Some(config) = manifest.config {
        properties.push(BrowseProperty {
            label: "Config".to_owned(),
            value: config.digest,
            href: None,
        });
    }
    BrowsePage {
        breadcrumbs: repository_breadcrumbs(route, repository),
        title: format!("{repository}:{reference}"),
        command: manifest.client_command,
        sections: vec![
            BrowseSection::Properties {
                heading: "Manifest".to_owned(),
                entries: properties,
            },
            BrowseSection::Table {
                heading: heading.to_owned(),
                columns: columns.into_iter().map(str::to_owned).collect(),
                rows,
                empty: format!("No {heading} found."),
            },
        ],
        ..BrowsePage::default()
    }
}

#[must_use]
pub fn members_page(route: &str, repository: &str, reference: &str, digest: &str, members: Vec<Member>) -> BrowsePage {
    BrowsePage {
        breadcrumbs: manifest_breadcrumbs(route, repository, reference),
        title: "Layer contents".to_owned(),
        subtitle: Some(digest.to_owned()),
        sections: vec![BrowseSection::Table {
            heading: "Members".to_owned(),
            columns: ["Path", "Size", "Kind"].into_iter().map(str::to_owned).collect(),
            rows: members
                .into_iter()
                .map(|member| BrowseRow {
                    cells: vec![
                        BrowseCell {
                            href: member.previewable.then(|| {
                                browse_url(
                                    route,
                                    &[
                                        ("project", repository),
                                        ("ref", reference),
                                        ("layer", digest),
                                        ("member", &member.path),
                                    ],
                                )
                            }),
                            text: member.path,
                            code: true,
                        },
                        BrowseCell {
                            text: member.size.to_string(),
                            ..BrowseCell::default()
                        },
                        BrowseCell {
                            text: member.kind,
                            ..BrowseCell::default()
                        },
                    ],
                    ..BrowseRow::default()
                })
                .collect(),
            empty: "No files found in this layer.".to_owned(),
        }],
        ..BrowsePage::default()
    }
}

#[must_use]
pub fn member_page(
    route: &str,
    repository: &str,
    reference: &str,
    digest: &str,
    member: &str,
    chunk: MemberChunk,
) -> BrowsePage {
    let next = chunk.next_offset.map(|offset| BrowseLink {
        label: "Next chunk".to_owned(),
        href: browse_url(
            route,
            &[
                ("project", repository),
                ("ref", reference),
                ("layer", digest),
                ("member", member),
                ("offset", &offset.to_string()),
            ],
        ),
    });
    BrowsePage {
        breadcrumbs: layer_breadcrumbs(route, repository, reference, digest),
        title: member.to_owned(),
        sections: vec![BrowseSection::Content {
            heading: "Preview".to_owned(),
            text: chunk.text,
            size: chunk.size,
            offset: chunk.offset,
            next,
        }],
        ..BrowsePage::default()
    }
}

fn manifest_content_from_json(value: &serde_json::Value) -> ManifestContent {
    let media_type = string_at(value, "mediaType");
    if let Some(children) = value["manifests"].as_array() {
        let entries: Vec<ManifestContentReference> = children.iter().map(content_reference).collect();
        return ManifestContent {
            media_type,
            is_index: true,
            config: None,
            total_size: saturating_total(entries.iter().map(|entry| entry.size)),
            entries,
            client_command: None,
        };
    }
    let config = value["config"].is_object().then(|| content_reference(&value["config"]));
    let entries: Vec<ManifestContentReference> = value["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(content_reference)
        .collect();
    ManifestContent {
        media_type,
        is_index: false,
        total_size: saturating_total(
            config
                .as_ref()
                .map(|entry| entry.size)
                .into_iter()
                .chain(entries.iter().map(|entry| entry.size)),
        ),
        config,
        entries,
        client_command: None,
    }
}

fn content_reference(value: &serde_json::Value) -> ManifestContentReference {
    let platform = value["platform"].is_object().then(|| {
        format!(
            "{}/{}",
            string_at(&value["platform"], "os"),
            string_at(&value["platform"], "architecture")
        )
    });
    let media_type = string_at(value, "mediaType");
    ManifestContentReference {
        digest: string_at(value, "digest"),
        size: value["size"].as_u64().unwrap_or_default(),
        browsable: media_type.contains("tar"),
        media_type,
        platform,
    }
}

fn manifest_row(
    route: &str,
    repository: &str,
    reference: &str,
    is_index: bool,
    entry: ManifestContentReference,
) -> BrowseRow {
    let href = if is_index {
        Some(browse_url(route, &[("project", repository), ("ref", &entry.digest)]))
    } else {
        entry.browsable.then(|| {
            browse_url(
                route,
                &[("project", repository), ("ref", reference), ("layer", &entry.digest)],
            )
        })
    };
    let mut cells = vec![BrowseCell {
        text: entry.digest,
        href: href.clone(),
        code: true,
    }];
    if is_index {
        cells.push(BrowseCell {
            text: entry.platform.unwrap_or_default(),
            ..BrowseCell::default()
        });
    }
    cells.extend([
        BrowseCell {
            text: entry.size.to_string(),
            ..BrowseCell::default()
        },
        BrowseCell {
            text: entry.media_type,
            ..BrowseCell::default()
        },
    ]);
    if !is_index {
        cells.push(BrowseCell {
            text: if entry.browsable { "contents" } else { "" }.to_owned(),
            href,
            code: false,
        });
    }
    BrowseRow {
        cells,
        ..BrowseRow::default()
    }
}

/// # Errors
/// Returns a message when the listing is not valid JSON.
pub fn members_from_bytes(bytes: &[u8]) -> Result<Vec<Member>, String> {
    serde_json::from_slice(bytes)
        .map(|value| members_from_listing(&value))
        .map_err(|error| error.to_string())
}

fn members_from_listing(value: &serde_json::Value) -> Vec<Member> {
    value["members"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|member| Member {
            path: string_at(member, "path"),
            size: member["size"].as_u64().unwrap_or_default(),
            kind: member["kind"].as_str().unwrap_or("unknown").to_owned(),
            previewable: member["previewable"].as_bool().unwrap_or(false),
        })
        .collect()
}

#[must_use]
pub fn header_u64(headers: &axum::http::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

fn repository_breadcrumbs(route: &str, repository: &str) -> Vec<BrowseLink> {
    vec![
        BrowseLink {
            label: route.to_owned(),
            href: browse_url(route, &[]),
        },
        BrowseLink {
            label: repository.to_owned(),
            href: browse_url(route, &[("project", repository)]),
        },
    ]
}

fn manifest_breadcrumbs(route: &str, repository: &str, reference: &str) -> Vec<BrowseLink> {
    let mut links = repository_breadcrumbs(route, repository);
    links.push(BrowseLink {
        label: reference.to_owned(),
        href: browse_url(route, &[("project", repository), ("ref", reference)]),
    });
    links
}

fn layer_breadcrumbs(route: &str, repository: &str, reference: &str, digest: &str) -> Vec<BrowseLink> {
    let mut links = manifest_breadcrumbs(route, repository, reference);
    links.push(BrowseLink {
        label: digest.to_owned(),
        href: browse_url(route, &[("project", repository), ("ref", reference), ("layer", digest)]),
    });
    links
}

fn browse_url(route: &str, pairs: &[(&str, &str)]) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("index", route);
    for (key, value) in pairs {
        query.append_pair(key, value);
    }
    format!("/browse?{}", query.finish())
}

fn saturating_total(sizes: impl Iterator<Item = u64>) -> u64 {
    sizes.fold(0, u64::saturating_add)
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
#[path = "../tests/unit/web/tests.rs"]
mod tests;
