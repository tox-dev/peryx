use std::io::{Read as _, Seek as _, SeekFrom};
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt as _;
use peryx_storage::blob::{BlobRead, BlobReadBody};

pub fn blob_read(read: BlobRead) -> Body {
    let length = read.range.end.saturating_sub(read.range.start);
    match read.body {
        BlobReadBody::File(file) => pipelined_file(file, read.range.start, length),
        BlobReadBody::Stream(stream) => Body::from_stream(stream),
    }
}

/// Invokes `complete` after at least `expected` bytes leave the body.
///
/// Length-framed responses may stop before polling the stream's terminating `None`, so completion
/// follows the byte count. An error or early EOF abandons the callback.
pub fn on_body_complete(body: Body, expected: u64, complete: impl FnOnce(u64) + Send + 'static) -> Body {
    on_body_complete_boxed(body, expected, Box::new(complete))
}

fn on_body_complete_boxed(body: Body, expected: u64, complete: Box<dyn FnOnce(u64) + Send>) -> Body {
    Body::from_stream(futures_util::stream::unfold(
        (body.into_data_stream(), Some(complete), 0u64),
        move |(mut stream, mut complete, bytes)| async move {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    let bytes = bytes.saturating_add(chunk.len() as u64);
                    if bytes >= expected
                        && let Some(complete) = complete.take()
                    {
                        complete(bytes);
                    }
                    Some((Ok(chunk), (stream, complete, bytes)))
                }
                Some(Err(error)) => {
                    complete = None;
                    Some((Err(error), (stream, complete, bytes)))
                }
                None => {
                    if bytes >= expected
                        && let Some(complete) = complete
                    {
                        complete(bytes);
                    }
                    None
                }
            }
        },
    ))
}

/// Streams a file range through a bounded channel of owned buffers.
///
/// Blocking reads overlap with hyper writes instead of serializing both I/O waits. The reader stops at
/// `length` or EOF. Read errors abort the response rather than serving truncated content.
pub fn pipelined_file(file: std::fs::File, offset: u64, length: u64) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);
    tokio::task::spawn_blocking(move || {
        let mut file = file;
        let mut positioned = offset == 0;
        let mut remaining = length;
        while remaining > 0 {
            let mut buffer = vec![0u8; remaining.min(1 << 20) as usize];
            let read = (|| {
                if !positioned {
                    file.seek(SeekFrom::Start(offset))?;
                    positioned = true;
                }
                file.read(&mut buffer)
            })();
            match read {
                Ok(0) => break,
                Ok(count) => {
                    buffer.truncate(count);
                    remaining -= count as u64;
                    if tx.blocking_send(Ok(Bytes::from(buffer))).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            }
        }
    });
    Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    }))
}

/// The error a stalled body ends with, worded for the client that stopped sending rather than for the
/// handler that was reading.
#[derive(Debug)]
pub struct Stalled(Duration);

impl std::error::Error for Stalled {}

impl std::fmt::Display for Stalled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the request body sent nothing for {:?}", self.0)
    }
}

impl Stalled {
    #[must_use]
    pub const fn new(after: Duration) -> Self {
        Self(after)
    }
}

/// Why reading a request body ended without the bytes the handler was waiting for.
///
/// The bytes of a request body come from the client, so no failure reading one is an upstream fault
/// and none of them may answer `502`. What the client should do next still differs, so the edge that
/// bounds the body is where the two are told apart: a handler holding an opaque body error cannot
/// recover the distinction, and every handler that tried would derive it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFailure {
    /// The client sent no frame for the bound. The request never completed, so it may be repeated,
    /// and a resumable session picks up at the offset its bytes reached.
    Stalled(Duration),
    /// The body stopped for some other reason: a dropped connection, or framing the server could not
    /// read. Repeating it unchanged fails the same way.
    Interrupted,
}

impl BodyFailure {
    /// Classify the error a request-body stream ended with.
    ///
    /// The stall arrives wrapped by whatever read the body, so the whole source chain is searched
    /// rather than the outermost error alone.
    #[must_use]
    pub fn of(error: &(dyn std::error::Error + 'static)) -> Self {
        let mut current = Some(error);
        while let Some(error) = current {
            if let Some(stalled) = error.downcast_ref::<Stalled>() {
                return Self::Stalled(stalled.0);
            }
            current = error.source();
        }
        Self::Interrupted
    }
}

#[cfg(test)]
#[path = "../tests/unit/body_failure_tests.rs"]
mod body_failure_tests;
