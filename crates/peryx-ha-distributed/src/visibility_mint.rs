//! The authority side of replicated visibility: minting typed trash, restore, revoke, and lift
//! transitions into the total order the pure apply core [`VisibilityState`](crate::VisibilityState)
//! converges from.
//!
//! [`VisibilityState::apply`](crate::VisibilityState::apply) resolves two operations that carry the
//! same [`OpOrder`] as a duplicate - the first applied wins - which is the right idempotent choice for
//! a resent operation but a silent divergence for two *different* actions that collide on one order. A
//! `Trash` and a `Restore` both stamped `(epoch 1, serial 5)` would resolve by arrival order, and two
//! replicas that receive them in opposite orders reach opposite visibilities from the same operation
//! set. The pure apply core cannot rule that out; only the authority that stamps the order can. This
//! minter is that authority. It draws every serial from a strictly monotonic source and stamps the
//! current authority epoch, so no two operations it produces ever share an [`OpOrder`] - the collision
//! the apply core cannot survive can never reach it.
//!
//! The uniqueness rests on two invariants the minter enforces at its boundary: the serial source never
//! returns a value it has returned before, even across a restart, and [`adopt_epoch`] only ever moves
//! the epoch upward. The production source is the metadata journal's persisted counter
//! ([`JournalSerials`]); a durable counter is what keeps a restarted home from reminting a serial a
//! prior operation already carried.

use peryx_storage::meta::{MetaError, MetaStore};

use crate::envelope::AuthorityEpoch;
use crate::visibility::{ArtifactId, OpOrder, VisibilityAction, VisibilityOp};

/// A strictly monotonic source of operation serials.
///
/// Each [`next_serial`](SerialSource::next_serial) returns a value greater than every value it has
/// returned before, so a serial is never reused - the property that keeps the minter's [`OpOrder`]s
/// unique.
pub trait SerialSource {
    /// The failure a serial draw can report.
    type Error;

    /// Draw the next serial, strictly greater than every value returned before.
    ///
    /// # Errors
    /// Returns [`Self::Error`] when the underlying source cannot allocate the next serial.
    fn next_serial(&mut self) -> Result<u64, Self::Error>;
}

/// The metadata journal's durable serial counter, the production authority for operation serials.
///
/// Its value is persisted, so a home that restarts resumes above every serial it has already minted
/// rather than replaying one under a new action.
#[derive(Debug)]
pub struct JournalSerials<'store> {
    store: &'store MetaStore,
}

impl<'store> JournalSerials<'store> {
    /// Draw serials from `store`'s durable journal counter.
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

/// An epoch a mint or [`adopt_epoch`](VisibilityMinter::adopt_epoch) would regress to, rejected so a
/// stale home cannot stamp an order under an epoch a newer home already superseded.
#[derive(Debug, thiserror::Error)]
#[error("epoch {presented} does not advance the minter's current epoch {current}")]
pub struct StaleEpoch {
    /// The epoch the minter currently stamps.
    pub current: u64,
    /// The epoch the caller tried to adopt, which did not advance the current one.
    pub presented: u64,
}

/// Stamps trash, restore, revoke, and lift transitions with a unique [`OpOrder`] under the current
/// authority epoch.
///
/// The pure [`VisibilityState`](crate::VisibilityState) never has to break a tie between two different
/// actions on one order, because the minter never produces one. A higher authority epoch always
/// outranks a lower one, so a transfer that advances the epoch through
/// [`adopt_epoch`](Self::adopt_epoch) fences every serial the prior home minted; within one epoch the
/// strictly monotonic [`SerialSource`] keeps each operation's serial distinct. Together the two rule
/// out any repeated `(epoch, serial)`.
#[derive(Debug)]
pub struct VisibilityMinter<S> {
    epoch: AuthorityEpoch,
    serials: S,
}

impl<S: SerialSource> VisibilityMinter<S> {
    /// Mint operations under `epoch`, drawing serials from `serials`.
    #[must_use]
    pub const fn new(epoch: AuthorityEpoch, serials: S) -> Self {
        Self { epoch, serials }
    }

    /// The authority epoch the next mint will stamp.
    #[must_use]
    pub const fn epoch(&self) -> AuthorityEpoch {
        self.epoch
    }

    /// Adopt a strictly higher authority epoch after a transfer, so later mints outrank every order the
    /// prior home produced.
    ///
    /// # Errors
    /// Returns [`StaleEpoch`] when `epoch` does not advance the current one, so a stale home that
    /// rejoined cannot pull the epoch back to one it already lost.
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

    /// Mint one visibility operation: draw a fresh serial and stamp the current epoch, producing an
    /// [`OpOrder`] no other operation from this authority shares.
    ///
    /// # Errors
    /// Returns `S::Error` when the serial source cannot allocate the next serial; nothing is stamped, so
    /// a later retry draws a fresh serial and no order is skipped in a way that reuses one.
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
