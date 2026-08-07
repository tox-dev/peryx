//! The availability control plane a `dc` or `ha` node exposes on a dedicated, authenticated listener.

mod dc_copy;
mod listener;
mod placement_reconcile;
mod reclamation;

pub use dc_copy::CrossDcBlobCopier;
pub use listener::{AVAILABILITY_PROTOCOL_VERSION, AvailabilityPosture, router};
pub use peryx_ha_distributed::{
    EpochOracle, FrontierSource, RosterFrontierSource, TransferCancelError, TransferCoordinator, TransferDriveError,
    TransferRunError, commit_transfer, observe_target,
};
pub use placement_reconcile::FilesystemPlacementReconciler;
pub use reclamation::BlobReclamationSelector;
