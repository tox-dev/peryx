//! Routing a blob fetch to the peer transport that holds the blob.
//!
//! Multi-source blob replication chooses a different peer per blob: the placement selection
//! ([`plan_blob_fetch`](crate::blob_placement::plan_blob_fetch)) names, for each digest, the datacenter to
//! fetch it from, and each peer datacenter has its own [`BlobTransport`]. The whole-blob scheduler
//! ([`fetch_missing`](crate::blob_fetch::fetch_missing)) drives ONE transport over the whole missing set.
//! This bridges the two: a [`BlobTransport`] that looks up each request's digest, finds the datacenter
//! chosen for it, and delegates to that datacenter's transport, so the scheduler keeps its
//! single-transport shape while every blob still fetches from its own source.
//!
//! It is generic over the delegate transport and holds only the routing decision (digest -> datacenter)
//! and the per-datacenter delegates. Turning placement plans into that routing, and resolving a datacenter
//! to a concrete HTTP transport, are the caller's wiring; this core only dispatches. A digest with no
//! chosen datacenter, or one whose datacenter has no delegate, fails closed with
//! [`TransportError::BlobNotFound`], so the scheduler treats an unroutable blob as a terminal miss rather
//! than a source to keep retrying.

use std::collections::HashMap;

use async_trait::async_trait;
use peryx_storage::blob::Digest;

use crate::blob::{BlobRequest, BlobTransport};
use crate::peer::TransportError;

/// A [`BlobTransport`] that dispatches each request to the delegate for the digest's chosen datacenter.
///
/// `routes` maps a digest to the datacenter selected to fetch it from; `delegates` maps a datacenter to
/// its transport. A request whose digest has no route, or whose datacenter has no delegate, fails closed
/// with [`TransportError::BlobNotFound`].
pub struct RoutingBlobTransport<T> {
    routes: HashMap<Digest, String>,
    delegates: HashMap<String, T>,
}

impl<T> RoutingBlobTransport<T> {
    /// Build a router from a digest-to-datacenter routing and the per-datacenter delegate transports.
    #[must_use]
    pub const fn new(routes: HashMap<Digest, String>, delegates: HashMap<String, T>) -> Self {
        Self { routes, delegates }
    }
}

#[async_trait]
impl<T: BlobTransport + Send> BlobTransport for RoutingBlobTransport<T> {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        match self.routes.get(&request.digest).and_then(|dc| self.delegates.get(dc)) {
            Some(delegate) => delegate.fetch_blob(request).await,
            None => Err(TransportError::BlobNotFound {
                digest: request.digest.as_str().to_owned(),
            }),
        }
    }
}
