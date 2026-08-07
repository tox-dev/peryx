//! A capacity-bounded channel between the drain that pulls changes from a peer and the apply step
//! that commits them.
//!
//! [`drain_to_frontier`] can outrun apply when a peer is fast, so an unbounded hand-off would let the
//! receiver grow without limit. This channel fixes the bound: [`try_push`](BoundedChannel::try_push)
//! fails closed with [`ChannelFull`] once the queue holds `capacity` changes, and
//! [`buffer_batch`] stops at the first rejection and reports back-pressure so the caller applies,
//! frees slots, and resumes from its persisted frontier rather than buffer the rest.
//!
//! The channel owns no runtime and moves no bytes of its own; it holds decoded [`Change`] records in
//! order, so a caller drives producer and consumer on whatever task structure it already has.
//!
//! [`drain_to_frontier`]: crate::peer::drain_to_frontier

use std::collections::VecDeque;
use std::num::NonZeroUsize;

use crate::protocol::Change;

/// The overflow signal: the channel is at capacity and rejects the push, so a fast producer cannot
/// grow the receiver past its configured bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("change channel is full at capacity {capacity}")]
pub struct ChannelFull {
    pub capacity: usize,
}

/// A bounded FIFO queue of decoded changes awaiting apply.
#[derive(Debug, Clone)]
pub struct BoundedChannel {
    queue: VecDeque<Change>,
    capacity: NonZeroUsize,
}

impl BoundedChannel {
    /// Build an empty channel that holds at most `capacity` changes.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity.get()),
            capacity,
        }
    }

    /// The most changes the channel holds at once.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity.get()
    }

    /// The number of changes currently queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the channel holds no changes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Whether the channel is at capacity and rejects further pushes until a [`pop`](Self::pop).
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.queue.len() == self.capacity.get()
    }

    /// Enqueue `change`, or fail closed with [`ChannelFull`] when the channel is at capacity.
    ///
    /// # Errors
    /// Returns [`ChannelFull`] without enqueuing when [`is_full`](Self::is_full) holds.
    pub fn try_push(&mut self, change: Change) -> Result<(), ChannelFull> {
        if self.is_full() {
            return Err(ChannelFull {
                capacity: self.capacity.get(),
            });
        }
        self.queue.push_back(change);
        Ok(())
    }

    /// Dequeue the oldest change, or `None` when the channel is empty.
    pub fn pop(&mut self) -> Option<Change> {
        self.queue.pop_front()
    }
}

/// The result of feeding a batch into a [`BoundedChannel`].
///
/// `accepted` counts the leading changes the channel took; `back_pressure` marks that it filled
/// before the batch drained, so `changes[accepted..]` still need a home after the caller frees slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferOutcome {
    pub accepted: usize,
    pub back_pressure: bool,
}

/// Push `changes` into `channel` in order until it fills, then stop and report back-pressure.
///
/// The caller keeps `changes`, applies and pops what the channel accepted, and resumes at `accepted`
/// once slots free. A full batch that fits leaves `back_pressure` clear.
#[must_use]
pub fn buffer_batch(channel: &mut BoundedChannel, changes: &[Change]) -> BufferOutcome {
    let mut accepted = 0;
    for change in changes {
        if channel.try_push(change.clone()).is_err() {
            return BufferOutcome {
                accepted,
                back_pressure: true,
            };
        }
        accepted += 1;
    }
    BufferOutcome {
        accepted,
        back_pressure: false,
    }
}
