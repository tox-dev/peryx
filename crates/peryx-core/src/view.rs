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

/// A project page: the files of one project on one index, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiProject {
    pub name: String,
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

/// One downloadable file as the project page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiFile {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
    pub yanked: bool,
    pub yanked_reason: Option<String>,
    pub has_metadata: bool,
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
