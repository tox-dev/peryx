use std::num::NonZeroU64;
use std::time::Duration;

use axum::body::{Body, Bytes};
use http_body::Body as _;
use peryx_core::ThroughputBudget;

use crate::request_bounds::BoundedBody;

#[test]
fn size_hint_passes_through_the_inner_bodys_exact_length() {
    let inner = Body::from(Bytes::from_static(b"hello world"));
    let bounded = BoundedBody::new(
        Duration::from_secs(30),
        ThroughputBudget::new(NonZeroU64::new(8 * 1024).unwrap(), Duration::from_secs(30)),
        inner,
    );

    assert_eq!(bounded.size_hint().exact(), Some(11));
}
