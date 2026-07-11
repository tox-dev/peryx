//! Core-metadata parsing: the `METADATA` document of a wheel (also served as the PEP 658 sibling),
//! RFC 822-style headers followed by an optional long-description body.

/// The fields of a core-metadata document that the web UI presents, in the spirit of a pypi.org
/// project page. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct CoreMetadataDoc {
    pub metadata_version: Option<String>,
    pub name: String,
    pub version: String,
    pub summary: Option<String>,
    pub requires_python: Option<String>,
    pub license: Option<String>,
    pub license_expression: Option<String>,
    pub license_files: Vec<String>,
    pub author: Option<String>,
    pub maintainer: Option<String>,
    pub keywords: Vec<String>,
    pub requires_dist: Vec<String>,
    pub provides_extra: Vec<String>,
    pub classifiers: Vec<String>,
    /// `(label, url)` pairs from `Project-URL` headers.
    pub project_urls: Vec<(String, String)>,
    /// The long description: the `Description` header or the document body.
    pub description: String,
    /// The `Description-Content-Type`, for example `text/markdown`.
    pub description_content_type: Option<String>,
}

/// Parse a core-metadata document.
#[must_use]
pub fn parse_metadata(text: &str) -> CoreMetadataDoc {
    let mut doc = CoreMetadataDoc::default();
    // The header/body boundary is a blank line, which is `\r\n\r\n` in a CRLF document; matching only
    // `\n\n` would read the whole CRLF document as headers and mis-parse body lines as fields.
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .unwrap_or((text, ""));
    for line in unfold(headers) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "metadata-version" => doc.metadata_version = non_empty(value),
            "name" => value.clone_into(&mut doc.name),
            "version" => value.clone_into(&mut doc.version),
            "summary" => doc.summary = non_empty(value),
            "requires-python" => doc.requires_python = non_empty(value),
            "license" => doc.license = non_empty(value),
            "license-expression" => {
                doc.license_expression = non_empty(value);
                if doc.license.is_none() {
                    doc.license = doc.license_expression.clone();
                }
            }
            "license-file" => doc.license_files.push(value.to_owned()),
            "author" | "author-email" => doc.author = doc.author.or_else(|| non_empty(value)),
            "maintainer" | "maintainer-email" => doc.maintainer = doc.maintainer.or_else(|| non_empty(value)),
            "keywords" => {
                doc.keywords = value
                    .split([',', ' '])
                    .filter(|keyword| !keyword.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            "requires-dist" => doc.requires_dist.push(value.to_owned()),
            "provides-extra" => doc.provides_extra.push(value.to_owned()),
            "classifier" => doc.classifiers.push(value.to_owned()),
            "project-url" => {
                let (label, url) = value.split_once(',').unwrap_or(("", value));
                doc.project_urls.push((label.trim().to_owned(), url.trim().to_owned()));
            }
            "home-page" => doc.project_urls.push(("Homepage".to_owned(), value.to_owned())),
            "description" => value.clone_into(&mut doc.description),
            "description-content-type" => doc.description_content_type = non_empty(value),
            _ => {}
        }
    }
    if doc.description.is_empty() {
        body.trim().clone_into(&mut doc.description);
    }
    doc
}

/// Join folded (indented continuation) header lines, per RFC 822 folding.
fn unfold(headers: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in headers.lines() {
        if (raw.starts_with(' ') || raw.starts_with('\t'))
            && let Some(last) = lines.last_mut()
        {
            last.push(' ');
            last.push_str(raw.trim_start());
            continue;
        }
        lines.push(raw.to_owned());
    }
    lines
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

impl CoreMetadataDoc {
    /// Turn this `PyPI` core-metadata document into the neutral view model the web UI renders, mapping
    /// each header to a metadata-panel block the way a pypi.org project page presents it.
    #[must_use]
    pub fn to_ui_meta(&self) -> peryx_core::UiMeta {
        use peryx_core::{UiBlock, UiDescription, UiMeta};

        let mut blocks = Vec::new();
        for (label, value) in [
            ("Requires Python", &self.requires_python),
            ("License", &self.license),
            ("Author", &self.author),
            ("Maintainer", &self.maintainer),
        ] {
            if let Some(value) = value {
                blocks.push(UiBlock::KeyValue {
                    label: label.to_owned(),
                    value: value.clone(),
                });
            }
        }
        for (label, values) in [("Keywords", &self.keywords), ("Dependencies", &self.requires_dist)] {
            if !values.is_empty() {
                blocks.push(UiBlock::Chips {
                    label: label.to_owned(),
                    values: values.clone(),
                });
            }
        }
        if !self.project_urls.is_empty() {
            blocks.push(UiBlock::Links {
                label: "Links".to_owned(),
                links: self.project_urls.clone(),
            });
        }
        if let Some(groups) = classifier_groups(&self.classifiers) {
            blocks.push(UiBlock::Groups {
                label: "Classifiers".to_owned(),
                groups,
            });
        }
        UiMeta {
            version: non_empty(&self.version),
            summary: self.summary.clone(),
            description: non_empty(&self.description).map(|text| UiDescription {
                text,
                content_type: self.description_content_type.clone(),
            }),
            blocks,
        }
    }
}

/// Group trove classifiers by their top-level `::`-separated category, the way pypi.org presents them.
/// `None` when there are none, so the caller emits no block.
fn classifier_groups(classifiers: &[String]) -> Option<Vec<(String, Vec<String>)>> {
    if classifiers.is_empty() {
        return None;
    }
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for classifier in classifiers {
        let (group, rest) = classifier
            .split_once(" :: ")
            .map_or((classifier.as_str(), classifier.as_str()), |(g, r)| (g, r));
        match groups.iter_mut().find(|(name, _)| name == group) {
            Some((_, values)) => values.push(rest.to_owned()),
            None => groups.push((group.to_owned(), vec![rest.to_owned()])),
        }
    }
    Some(groups)
}

/// Parse a `PyPI` core-metadata document straight into the neutral [`UiMeta`](peryx_core::UiMeta) the
/// web UI renders.
#[must_use]
pub fn ui_meta(metadata_text: &str) -> peryx_core::UiMeta {
    parse_metadata(metadata_text).to_ui_meta()
}

/// Build a neutral [`UiProject`](peryx_core::UiProject) from a PEP 691 project-detail JSON document.
///
/// This is the shape the web project page renders. The `PyPI`-specific field names (`core-metadata`,
/// PEP 592 `yanked`) are read here so the UI never sees them.
#[must_use]
pub fn ui_project_from_detail(value: &serde_json::Value) -> peryx_core::UiProject {
    fn string_at(value: &serde_json::Value, key: &str) -> String {
        value[key].as_str().unwrap_or_default().to_owned()
    }
    let files = value["files"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| peryx_core::UiFile {
            filename: string_at(file, "filename"),
            url: string_at(file, "url"),
            sha256: file["hashes"]["sha256"].as_str().unwrap_or_default().to_owned(),
            size: file["size"].as_u64(),
            upload_time: file["upload-time"].as_str().map(str::to_owned),
            yanked: file["yanked"].as_bool().unwrap_or(false) || file["yanked"].is_string(),
            has_metadata: file["core-metadata"].is_object() || file["core-metadata"].as_bool() == Some(true),
        })
        .collect();
    peryx_core::UiProject {
        name: string_at(value, "name"),
        versions: value["versions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|version| version.as_str().map(str::to_owned))
            .collect(),
        files,
    }
}
