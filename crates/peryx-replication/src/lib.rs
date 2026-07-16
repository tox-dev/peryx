//! Primary/replica replication over peryx's ordered storage journal.
//!
//! A primary exposes [`ChangePage`] records and digest-addressed blob streams through [`Primary`].
//! [`Replica`] verifies the serial sequence and every missing blob before committing metadata,
//! copied journal entries, and its resume cursor in one transaction.

mod error;
mod protocol;
mod replica;

pub use error::SyncError;
pub use protocol::{BlobReference, Change, ChangePage, MetadataMutation, PROTOCOL_VERSION, Primary};
pub use replica::{Replica, ReplicaState, SyncOutcome};

#[cfg(test)]
mod tests;
