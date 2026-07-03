//! Core-metadata parsing: the `METADATA` document of a wheel (also served as the PEP 658 sibling),
//! RFC 822-style headers followed by an optional long-description body.

/// The fields of a core-metadata document that the web UI presents, in the spirit of a pypi.org
/// project page. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct CoreMetadataDoc {
    pub name: String,
    pub version: String,
    pub summary: Option<String>,
    pub requires_python: Option<String>,
    pub license: Option<String>,
    pub author: Option<String>,
    pub maintainer: Option<String>,
    pub keywords: Vec<String>,
    pub requires_dist: Vec<String>,
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
    let (headers, body) = text.split_once("\n\n").unwrap_or((text, ""));
    for line in unfold(headers) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "name" => value.clone_into(&mut doc.name),
            "version" => value.clone_into(&mut doc.version),
            "summary" => doc.summary = non_empty(value),
            "requires-python" => doc.requires_python = non_empty(value),
            "license" | "license-expression" => doc.license = non_empty(value),
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
