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
    /// The long description and how to render it.
    pub description: Option<UiDescription>,
    /// The metadata-panel blocks, in display order. Each is a neutral presentation primitive an
    /// ecosystem filled; the page renders the vocabulary without knowing which format produced it.
    pub blocks: Vec<UiBlock>,
}

/// A long description and the content type that decides how it renders (markdown vs preformatted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiDescription {
    pub text: String,
    pub content_type: Option<String>,
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
    pub versions: Vec<String>,
    pub files: Vec<UiFile>,
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
    pub has_metadata: bool,
}
