//! Ecosystem-neutral domain core for peryx.
//!
//! This crate is pure: no I/O, no async runtime, no storage dependency, so its logic is fast and
//! deterministic to test.
//!
//! It owns the two axes an index is classified by: the [`Role`] it plays and the opaque [`Ecosystem`]
//! identifier it speaks. Implementations register against these contracts without changing core.

pub mod contracts;
pub mod ecosystem;
pub mod lexicon;
pub mod lifecycle;
pub mod operations;
pub mod path;
pub mod placement;
pub mod plugin;
pub mod role;
pub mod shadow;
pub mod topology;
pub mod url_encoding;
pub mod view;

pub use contracts::EcosystemInstaller;
pub use ecosystem::{Ecosystem, InvalidEcosystem};
pub use lexicon::{Lexicon, LexiconRegistry};
pub use lifecycle::{TRASH_GRACE_SECS, TrashInfo, TrashRecord, TrashState, UnknownTrashState};
pub use operations::{OperationRow, OperationsHealth, OperationsView};
pub use placement::{
    BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView, PlacementHealth, PlacementRow, PlacementView,
};
pub use plugin::{DefaultIndex, DefaultIndexKind};
pub use role::Role;
pub use shadow::{ShadowCandidate, ShadowReason, ShadowSource};
pub use topology::{
    LocalNode, LocalStatus, MAX_TOPOLOGY_NODES, NodeLiveness, NodeRole, TopologyConfig, TopologyMember, TopologyMode,
    TopologyNode, TopologySnapshot, TopologyView,
};
pub use view::{
    RenderedDescription, UiArtifactRef, UiArtifactSource, UiAttestation, UiBlock, UiByteAvailability, UiFile,
    UiManifest, UiMember, UiMemberChunk, UiMeta, UiOperationStatus, UiProject, UiProjectStatus, UiProjectView,
    UiProvenance, UiProvenanceSource, UiRelease, UiSubjectMatch, UiUploadSpec,
};
