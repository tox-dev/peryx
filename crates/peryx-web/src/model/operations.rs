//! The pending-operations-health view models, shared by the server renderer and the hydrated client.
//!
//! The neutral [`OperationsView`] crosses the server/browser boundary unchanged; each row's status reads
//! as a labelled health cell whose word stands alone, so a color-blind reader loses nothing.

pub use peryx_core::{OperationRow, OperationsHealth, OperationsView, UiOperationStatus};

use super::HealthLabel;

/// An operation's client-facing status as a labelled health cell.
///
/// The word an administrator reads plus the css class that tints it. The word stands alone, so a
/// color-blind reader loses nothing, and a write that has not settled never borrows the published tint.
#[must_use]
pub const fn operation_status_label(status: UiOperationStatus) -> HealthLabel {
    match status {
        UiOperationStatus::Published => HealthLabel {
            text: "Published",
            class: "health-live",
        },
        UiOperationStatus::Pending => HealthLabel {
            text: "Pending",
            class: "health-unready",
        },
        UiOperationStatus::Failed => HealthLabel {
            text: "Failed",
            class: "health-unknown",
        },
        UiOperationStatus::Expired => HealthLabel {
            text: "Expired",
            class: "health-restricted",
        },
    }
}
