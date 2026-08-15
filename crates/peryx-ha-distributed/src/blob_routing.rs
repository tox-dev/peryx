//! Adapts [`plan_blob_fetch`](crate::blob_placement::plan_blob_fetch) per-digest source selection to
//! [`fetch_missing`](crate::blob_fetch::fetch_missing), which accepts one transport for the missing set.

use std::collections::HashMap;

use async_trait::async_trait;
use peryx_storage::blob::Digest;

use crate::blob::{BlobRequest, BlobTransport};
use crate::peer::TransportError;

/// Missing routes and delegates return [`TransportError::BlobNotFound`].
pub struct RoutingBlobTransport<T> {
    routes: HashMap<Digest, String>,
    delegates: HashMap<String, T>,
}

impl<T> RoutingBlobTransport<T> {
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
