//! Streaming a stored file to the client with the disk read pipelined ahead of the socket write.

use std::io::{Read as _, Seek as _, SeekFrom};

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt as _;
use peryx_storage::blob::{BlobRead, BlobReadBody};

/// Preserve the selected backend's streaming representation in an HTTP body.
pub fn blob_read(read: BlobRead) -> Body {
    let length = read.range.end.saturating_sub(read.range.start);
    match read.body {
        BlobReadBody::File(file) => pipelined_file(file, read.range.start, length),
        BlobReadBody::Stream(stream) => Body::from_stream(stream),
    }
}

/// Run `complete` with the transmitted byte count only after a body reaches a clean EOF.
pub fn on_body_complete(body: Body, complete: impl FnOnce(u64) + Send + 'static) -> Body {
    Body::from_stream(futures_util::stream::unfold(
        (body.into_data_stream(), Some(complete), 0u64),
        |(mut stream, mut complete, bytes)| async move {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    let bytes = bytes.saturating_add(chunk.len() as u64);
                    Some((Ok(chunk), (stream, complete, bytes)))
                }
                Some(Err(error)) => {
                    complete = None;
                    Some((Err(error), (stream, complete, bytes)))
                }
                None => {
                    if let Some(complete) = complete {
                        complete(bytes);
                    }
                    None
                }
            }
        },
    ))
}

/// Stream a file with the disk read running ahead of the socket write.
///
/// A blocking reader fills a small channel of owned buffers while hyper drains it, so the read and the
/// write overlap instead of alternating: a pull-driven `ReaderStream` awaits each read to complete
/// before writing that chunk, serializing two independent I/O waits. `offset`/`length` select the byte
/// range to serve (`0` and the file length for a whole file); the reader also stops at EOF, so a
/// `length` past the end is harmless. A read error poisons the stream so hyper aborts the response
/// rather than serving a silently truncated body.
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
