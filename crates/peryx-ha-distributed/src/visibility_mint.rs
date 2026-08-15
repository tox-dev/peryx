//! Authority-side ordering for replicated visibility operations.
//!
//! Distinct actions with the same [`OpOrder`] would converge by arrival order. The minter prevents that
//! collision with durable, monotonic serials and strict epoch increases.

use peryx_storage::meta::{MetaError, MetaStore};

use crate::envelope::AuthorityEpoch;
use crate::visibility::{ArtifactId, OpOrder, VisibilityAction, VisibilityOp};

/// Successful draws must increase across restarts and cannot reuse a serial.
pub trait SerialSource {
    type Error;

    /// # Errors
    /// Returns the source error when it cannot reserve the next serial.
    fn next_serial(&mut self) -> Result<u64, Self::Error>;
}

/// Uses the durable journal counter so restarts cannot reuse a minted serial.
#[derive(Debug)]
pub struct JournalSerials<'store> {
    store: &'store MetaStore,
}

impl<'store> JournalSerials<'store> {
    #[must_use]
    pub const fn new(store: &'store MetaStore) -> Self {
        Self { store }
    }
}

impl SerialSource for JournalSerials<'_> {
    type Error = MetaError;

    fn next_serial(&mut self) -> Result<u64, Self::Error> {
        self.store.next_serial()
    }
}

/// Rejects an epoch that would let a stale authority supersede a newer one.
#[derive(Debug, thiserror::Error)]
#[error("epoch {presented} does not advance the minter's current epoch {current}")]
pub struct StaleEpoch {
    pub current: u64,
    pub presented: u64,
}

/// Produces unique `(epoch, serial)` orders and fences prior authorities after an epoch advance.
#[derive(Debug)]
pub struct VisibilityMinter<S> {
    epoch: AuthorityEpoch,
    serials: S,
}

impl<S: SerialSource> VisibilityMinter<S> {
    #[must_use]
    pub const fn new(epoch: AuthorityEpoch, serials: S) -> Self {
        Self { epoch, serials }
    }

    #[must_use]
    pub const fn epoch(&self) -> AuthorityEpoch {
        self.epoch
    }

    /// Requires a strict epoch increase so later mints fence the prior authority.
    ///
    /// # Errors
    /// Returns [`StaleEpoch`] when `epoch` does not advance the current one.
    pub fn adopt_epoch(&mut self, epoch: AuthorityEpoch) -> Result<(), StaleEpoch> {
        if epoch <= self.epoch {
            return Err(StaleEpoch {
                current: self.epoch.0,
                presented: epoch.0,
            });
        }
        self.epoch = epoch;
        Ok(())
    }

    /// Stamps the current epoch with a fresh serial.
    ///
    /// # Errors
    /// Returns `S::Error` when the serial source cannot allocate a serial.
    pub fn mint(&mut self, artifact: ArtifactId, action: VisibilityAction) -> Result<VisibilityOp, S::Error> {
        let serial = self.serials.next_serial()?;
        Ok(VisibilityOp {
            artifact,
            action,
            order: OpOrder {
                epoch: self.epoch.0,
                serial,
            },
        })
    }
}
