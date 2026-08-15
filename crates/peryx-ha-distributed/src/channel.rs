//! Bounds decoded changes waiting between peer fetch and apply. A full channel rejects new changes;
//! callers must apply queued changes and resume from the persisted frontier.

use std::collections::VecDeque;
use std::num::NonZeroUsize;

use crate::protocol::Change;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("change channel is full at capacity {capacity}")]
pub struct ChannelFull {
    pub capacity: usize,
}

#[derive(Debug, Clone)]
pub struct BoundedChannel {
    queue: VecDeque<Change>,
    capacity: NonZeroUsize,
}

impl BoundedChannel {
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity.get()),
            capacity,
        }
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity.get()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    #[must_use]
    pub fn is_full(&self) -> bool {
        self.queue.len() == self.capacity.get()
    }

    /// # Errors
    /// Returns [`ChannelFull`] without enqueueing when the channel is full.
    pub fn try_push(&mut self, change: Change) -> Result<(), ChannelFull> {
        if self.is_full() {
            return Err(ChannelFull {
                capacity: self.capacity.get(),
            });
        }
        self.queue.push_back(change);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Change> {
        self.queue.pop_front()
    }
}

/// `accepted` counts the leading changes queued. When `back_pressure` is set,
/// `changes[accepted..]` remain unqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferOutcome {
    pub accepted: usize,
    pub back_pressure: bool,
}

/// Preserves order and stops at the first full-channel rejection. Callers retain `changes` and resume
/// at `accepted` after freeing capacity.
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
