//! Ecosystem-neutral domain core for peryx.
//!
//! This crate is pure: no I/O, no async runtime, no storage dependency, so its logic is fast and
//! deterministic to test.
//!
//! It owns the two axes an index is classified by — the [`Role`] it plays and the [`Ecosystem`] it
//! speaks — plus the neutral [`Lexicon`]. peryx implements the Python and OCI ecosystems today
//! (the `peryx-ecosystem-*` crates); further ones (npm, crates, …) are sibling crates that add an
//! [`Ecosystem`] variant and register a serving driver, without reworking the crates that depend on
//! this one.

pub mod ecosystem;
pub mod lexicon;
pub mod path;
pub mod role;
pub mod url_encoding;
pub mod view;

pub use ecosystem::{Ecosystem, UnknownEcosystem};
pub use lexicon::{Lexicon, LexiconRegistry};
pub use role::Role;
pub use view::{
    UiArtifactRef, UiBlock, UiDescription, UiFile, UiManifest, UiMember, UiMemberChunk, UiMeta, UiProject,
    UiProjectView,
};
