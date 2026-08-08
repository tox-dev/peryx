use std::num::NonZeroUsize;

use crate::channel::{BoundedChannel, BufferOutcome, ChannelFull, buffer_batch};
use crate::protocol::Change;

fn channel(capacity: usize) -> BoundedChannel {
    BoundedChannel::new(NonZeroUsize::new(capacity).unwrap())
}

fn change(serial: u64) -> Change {
    Change {
        serial,
        event: serial.to_le_bytes().to_vec(),
        metadata: Vec::new(),
        blobs: Vec::new(),
    }
}

#[test]
fn test_new_channel_is_empty_with_the_configured_capacity() {
    let channel = channel(2);

    assert_eq!(channel.capacity(), 2);
    assert_eq!(channel.len(), 0);
    assert!(channel.is_empty());
    assert!(!channel.is_full());
}

#[test]
fn test_push_accepts_up_to_capacity_then_fails_closed() {
    let mut channel = channel(2);

    assert_eq!(channel.try_push(change(1)), Ok(()));
    assert_eq!(channel.try_push(change(2)), Ok(()));
    assert!(channel.is_full());
    assert_eq!(channel.len(), 2);

    assert_eq!(channel.try_push(change(3)), Err(ChannelFull { capacity: 2 }));
    assert_eq!(channel.len(), 2);
}

#[test]
fn test_pop_returns_the_oldest_change_and_frees_a_slot() {
    let mut channel = channel(1);
    channel.try_push(change(7)).unwrap();
    assert!(channel.is_full());

    assert_eq!(channel.pop(), Some(change(7)));
    assert!(channel.is_empty());

    channel.try_push(change(8)).unwrap();
    assert_eq!(channel.len(), 1);
}

#[test]
fn test_pop_on_an_empty_channel_returns_none() {
    let mut channel = channel(1);

    assert_eq!(channel.pop(), None);
}

#[test]
fn test_buffer_batch_accepts_a_batch_that_fits() {
    let mut channel = channel(3);

    let outcome = buffer_batch(&mut channel, &[change(1), change(2)]);

    assert_eq!(
        outcome,
        BufferOutcome {
            accepted: 2,
            back_pressure: false,
        }
    );
    assert_eq!(channel.len(), 2);
}

#[test]
fn test_buffer_batch_signals_back_pressure_when_the_channel_fills() {
    let mut channel = channel(2);

    let outcome = buffer_batch(&mut channel, &[change(1), change(2), change(3)]);

    assert_eq!(
        outcome,
        BufferOutcome {
            accepted: 2,
            back_pressure: true,
        }
    );
    assert!(channel.is_full());
}

#[test]
fn test_buffer_batch_on_an_empty_batch_accepts_nothing() {
    let mut channel = channel(2);

    let outcome = buffer_batch(&mut channel, &[]);

    assert_eq!(
        outcome,
        BufferOutcome {
            accepted: 0,
            back_pressure: false,
        }
    );
}
