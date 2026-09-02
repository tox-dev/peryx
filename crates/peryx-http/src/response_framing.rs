//! What a `304 Not Modified` is allowed to say about the length of the representation it validates.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use http_body::{Frame, SizeHint};

/// Send no `Content-Length` on a `304`, whatever the handler and the framework would have put there.
///
/// RFC 9112 s6.2 admits the field on a `304` only at the value a `200` to the same request would
/// carry, and RFC 9111 s4.3.4 writes a `304`'s fields into the response a cache already holds, so a
/// length that is not the representation's outlives the exchange that carried it. A `304` transfers
/// no body, and the zero its empty body measures is the length of no artifact peryx serves.
///
/// Omitting the field is the other answer RFC 9112 s6.2 allows, and the only one peryx can give
/// consistently, because hyper will not carry the representation's own length here: over HTTP/1.1 it
/// drops a `Content-Length` whose body has already ended, and over HTTP/2 a stated length no `DATA`
/// frame backs is a `PROTOCOL_ERROR` its own client resets the stream on. A cache that needs the
/// length reuses the one it stored from the `200`, which is the value it would have been sent.
///
/// axum stamps the body's exact size onto every top-level response, gated on `CONNECT` and `HEAD`
/// alone, so removing the field is not enough on its own: the body has to state no size either, or
/// the stamp lands after this runs. `Ended` states no size while still ending the message, which
/// leaves both protocols framing the `304` the way they frame any bodyless response.
pub fn frame_not_modified(response: &mut Response) {
    if response.status() == StatusCode::NOT_MODIFIED {
        response.headers_mut().remove(header::CONTENT_LENGTH);
        *response.body_mut() = Body::new(Ended);
    }
}

/// A body that is over and admits to no length, so nothing downstream infers one from it.
struct Ended;

impl http_body::Body for Ended {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        true
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}
