//! Neutral view models the web UI renders.
//!
//! The UI is ecosystem-agnostic: it lays out a page but knows nothing about wheels, core metadata or
//! `PyPI` headers. Each ecosystem crate turns its own format into these neutral shapes, and the web
//! crate renders them. The models are pure serde with no rendering or I/O, so they cross the
//! server/browser boundary and pull no UI toolkit into an ecosystem crate.
//!
//! The metadata panel is a list of [`UiBlock`]s — a small vocabulary of presentation primitives keyed
//! by *shape* (a key/value, a chip set, a link list), never by ecosystem. An ecosystem composes those
//! primitives to describe its own format, so a new ecosystem adds no field here and no branch in the
//! web crate. [`UiBlock`] is `#[non_exhaustive]`: a genuinely new primitive is one additive variant
//! plus one match arm in the renderer, and the renderer's catch-all keeps an unknown block from
//! silently rendering nothing. This is the server-driven-UI shape Airbnb's section union and Sanity's
//! Portable Text use, sized down to what a package page needs.

use serde::{Deserialize, Serialize};

/// A project's descriptive metadata, ready for a page to render without knowing the ecosystem it came
/// from. An ecosystem driver fills what its format has; the rest stay empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiMeta {
    /// The newest version, when the format names one distinctly from the file list.
    pub version: Option<String>,
    /// A one-line summary shown under the title.
    pub summary: Option<String>,
    /// The long description rendered to sanitized HTML, produced on the server so the browser shows it
    /// without running the renderer. Rendering reStructuredText in the browser can abort the
    /// WebAssembly module on constructs the renderer never implemented, and that abort cannot be
    /// caught there, so the render happens once, in the ecosystem driver, where a panic is recoverable.
    pub description: Option<RenderedDescription>,
    /// The metadata-panel blocks, in display order. Each is a neutral presentation primitive an
    /// ecosystem filled; the page renders the vocabulary without knowing which format produced it.
    pub blocks: Vec<UiBlock>,
}

/// A description rendered to safe HTML, with the message to show when rendering fell back to plain
/// text. The ecosystem driver produces it so the renderer runs server-side, in one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RenderedDescription {
    pub html: String,
    pub notice: Option<String>,
}

/// One block of a metadata panel: a presentation primitive keyed by shape, not by ecosystem.
///
/// `#[non_exhaustive]`, so a new primitive is additive — a variant here plus a match arm in the web
/// renderer, whose catch-all keeps an unrecognized block from rendering as a blank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
#[non_exhaustive]
pub enum UiBlock {
    /// A single labelled value (requires-python, license, author).
    KeyValue { label: String, value: String },
    /// A labelled set of short values shown as chips (keywords, dependencies).
    Chips { label: String, values: Vec<String> },
    /// A labelled list of links (`(text, url)` pairs, such as project URLs).
    Links {
        label: String,
        links: Vec<(String, String)>,
    },
    /// A labelled set of named groups, each a list of values (trove classifiers by category).
    Groups {
        label: String,
        groups: Vec<(String, Vec<String>)>,
    },
}

/// A project's publish status, when its index flags the project as archived, quarantined, or
/// deprecated.
///
/// The ecosystem driver fills it only for a flagged project, so an active or unmarked one carries
/// `None` and the page shows no badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiProjectStatus {
    /// The status marker, lowercased (`archived`, `quarantined`, `deprecated`). It names the badge and
    /// keys its style, the way the ecosystem and kind chips do, so a marker the page has no style for
    /// still renders as a plain badge.
    pub marker: String,
    /// The publisher's explanation for the status. Package-supplied text, so the page escapes it.
    pub reason: Option<String>,
}

/// A project page: the files of one project on one index, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiProject {
    pub name: String,
    /// The publish status to flag beside the heading, or `None` for a project served as usual. Boxed so
    /// this rare field does not enlarge the shared `UiProjectView` for every project.
    pub status: Option<Box<UiProjectStatus>>,
    pub versions: Vec<UiRelease>,
    pub files: Vec<UiFile>,
}

/// One release of a project: a version and the yank state its files give it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiRelease {
    pub version: String,
    /// Whether the publisher yanked the whole release. A release keeping one usable file is active.
    pub yanked: bool,
    /// The reasons the publisher gave, distinct and in the order the index lists them. Empty when the
    /// release is active or the publisher gave no reason.
    pub yanked_reasons: Vec<String>,
}

/// Where the artifact a file names lives, relative to this instance.
///
/// The badge a package page shows for a file, and the axis its `Local only` filter cuts on. `Hosted`
/// and `Cached` are both served from local storage; `RemoteOnly` is an upstream catalog entry whose
/// blob this instance has not downloaded, so a `Local only` view drops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiAvailability {
    /// Uploaded into this instance.
    Hosted,
    /// Mirrored from upstream: the artifact blob is in local storage.
    Cached,
    /// Present in the upstream catalog, but the blob is not stored locally.
    RemoteOnly,
}

/// One downloadable file as the project page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiFile {
    pub filename: String,
    /// The declared release this file belongs to, after ecosystem-specific version matching. `None`
    /// keeps a file visible when its filename is malformed, its release is undeclared, or several
    /// declared releases normalize to the same identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
    pub yanked: bool,
    pub yanked_reason: Option<String>,
    pub has_metadata: bool,
    /// The configured upstream source that advertised this artifact, when routing is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The PEP 740 provenance URL this file advertises, exactly as the index published it. `None` when
    /// the file names no provenance, spells it as an explicit `null`, or gives an empty URL.
    ///
    /// This carries the advertised location only: peryx neither fetches the document nor verifies the
    /// attestation it wraps, so its presence attests that the publisher claimed provenance, not that
    /// any signature was checked. The renderer applies the page's URL-scheme and external-link policy
    /// before it becomes a link, so an unsafe value is dropped without hiding the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    /// Whether the artifact is served from local storage (hosted or cached) or remains upstream-only.
    pub availability: UiAvailability,
}

/// What a project-level browse request renders as.
///
/// Chosen by the ecosystem driver so the web crate dispatches without naming a format. A file-based
/// ecosystem returns its file listing and descriptive metadata; a registry returns its list of
/// references, each resolving to a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum UiProjectView {
    /// A file listing with descriptive metadata (a `PyPI` project page).
    Files { project: UiProject, meta: UiMeta },
    /// A list of named references, each resolving to a manifest (an `OCI` repository's tags).
    References { names: Vec<String> },
}

/// One referenced content item in a manifest view.
///
/// A primary blob, a listed blob, or a per-platform child of an index: its digest, size, and content
/// type, an optional platform tag, and whether the web browser can list its contents. The driver
/// decides `browsable`, so shared code never inspects a content type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiArtifactRef {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    /// `os/architecture` when this entry is a per-platform child of an index.
    pub platform: Option<String>,
    /// Whether the layer browser can list this entry's contents.
    pub browsable: bool,
}

/// A manifest view, neutral so the web crate renders it without parsing any wire format.
///
/// A content type and total size, an optional primary item (a config) and a list of referenced items
/// (layers), or a flag that it is an index of per-platform child manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiManifest {
    pub media_type: String,
    pub is_index: bool,
    pub config: Option<UiArtifactRef>,
    /// Listed items: the layers of a manifest, or the per-platform children of an index.
    pub entries: Vec<UiArtifactRef>,
    pub total_size: u64,
}

/// One member of a nested content item (a distribution archive or an image layer), as a browser lists
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiMember {
    pub path: String,
    pub size: u64,
    pub kind: String,
    pub previewable: bool,
}

/// One rendered chunk of a nested content member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiMemberChunk {
    pub text: String,
    pub size: Option<u64>,
    pub offset: u64,
    pub next_offset: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{UiAvailability, UiFile};

    #[test]
    fn test_ui_availability_round_trips_snake_case() {
        for (availability, wire) in [
            (UiAvailability::Hosted, "\"hosted\""),
            (UiAvailability::Cached, "\"cached\""),
            (UiAvailability::RemoteOnly, "\"remote_only\""),
        ] {
            assert_eq!(serde_json::to_string(&availability).unwrap(), wire);
            assert_eq!(serde_json::from_str::<UiAvailability>(wire).unwrap(), availability);
        }
    }

    #[test]
    fn test_ui_file_carries_availability_on_the_wire() {
        let file = UiFile {
            filename: "pkg-1.0-py3-none-any.whl".to_owned(),
            release: Some("1.0".to_owned()),
            url: "/pypi/files/aa/pkg-1.0-py3-none-any.whl".to_owned(),
            sha256: "aa".to_owned(),
            size: Some(10),
            upload_time: None,
            yanked: false,
            yanked_reason: None,
            has_metadata: false,
            upstream: Some("mirror".to_owned()),
            provenance: Some("https://pypi.example/files/aa/pkg-1.0-py3-none-any.whl.provenance".to_owned()),
            availability: UiAvailability::RemoteOnly,
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains("\"availability\":\"remote_only\""), "{json}");
        assert!(json.contains("\"upstream\":\"mirror\""), "{json}");
        assert!(json.contains("\"release\":\"1.0\""), "{json}");
        assert!(
            json.contains("\"provenance\":\"https://pypi.example/files/aa/pkg-1.0-py3-none-any.whl.provenance\""),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<UiFile>(&json).unwrap(), file);
    }

    #[test]
    fn test_ui_file_omits_absent_provenance_from_the_wire() {
        let file = UiFile {
            filename: "pkg-1.0-py3-none-any.whl".to_owned(),
            release: None,
            url: "/pypi/files/aa/pkg-1.0-py3-none-any.whl".to_owned(),
            sha256: "aa".to_owned(),
            size: None,
            upload_time: None,
            yanked: false,
            yanked_reason: None,
            has_metadata: false,
            upstream: None,
            provenance: None,
            availability: UiAvailability::RemoteOnly,
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(!json.contains("provenance"), "{json}");
        assert_eq!(serde_json::from_str::<UiFile>(&json).unwrap().provenance, None);
    }

    #[test]
    fn test_ui_file_defaults_an_omitted_release_to_unassociated() {
        let file: UiFile = serde_json::from_value(serde_json::json!({
            "filename": "notes.txt",
            "url": "/files/notes.txt",
            "sha256": "aa",
            "size": null,
            "upload_time": null,
            "yanked": false,
            "yanked_reason": null,
            "has_metadata": false,
            "availability": "remote_only",
        }))
        .unwrap();

        assert_eq!(file.release, None);
    }
}
