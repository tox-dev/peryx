//! `PyPI` distribution-archive validation and metadata extraction.
//!
//! The wheel and sdist correctness checks run at upload time, and the PEP 658 `METADATA`/`PKG-INFO`
//! sidecar extraction, layer the format-specific rules on the ecosystem-neutral archive engine in
//! `peryx-storage` that the web UI's file browser drives for every ecosystem.

mod sdist;
mod wheel;

pub use peryx_storage::archive::*;

pub use sdist::{sdist_metadata_path, validate_sdist_path};
pub use wheel::{
    MAX_WHEEL_METADATA_BYTES, validate_wheel_path, wheel_metadata, wheel_metadata_member_path, wheel_metadata_path,
};
