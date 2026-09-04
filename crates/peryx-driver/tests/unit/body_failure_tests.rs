//! Telling a stalled body apart from any other way one ends.

use std::time::Duration;

use super::{BodyFailure, Stalled};

#[derive(Debug)]
struct Wrapper(Box<dyn std::error::Error + Send + Sync>);

impl std::error::Error for Wrapper {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

impl std::fmt::Display for Wrapper {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "reading the body failed")
    }
}

/// A handler never sees the stall itself: whatever read the body wraps it, so the chain is what the
/// classification has to walk.
#[test]
fn test_a_stall_is_recognized_through_the_reader_that_wrapped_it() {
    let stalled = Wrapper(Box::new(Stalled::new(Duration::from_secs(30))));

    assert_eq!(BodyFailure::of(&stalled), BodyFailure::Stalled(Duration::from_secs(30)));
}

#[test]
fn test_a_bare_stall_is_recognized() {
    assert_eq!(
        BodyFailure::of(&Stalled::new(Duration::from_secs(5))),
        BodyFailure::Stalled(Duration::from_secs(5))
    );
}

/// Everything else a request body ends with is still the client's side of the exchange, so it reads
/// as interrupted rather than as anything upstream.
#[test]
fn test_any_other_failure_reads_as_interrupted() {
    let broken = Wrapper(Box::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset)));

    assert_eq!(BodyFailure::of(&broken), BodyFailure::Interrupted);
}

/// The stall reaches a log through this wording, so it says which end went quiet and how long it
/// waited rather than naming the reader that surfaced it.
#[test]
fn test_a_stall_says_how_long_it_waited() {
    assert_eq!(
        Stalled::new(Duration::from_secs(30)).to_string(),
        "the request body sent nothing for 30s"
    );
}
