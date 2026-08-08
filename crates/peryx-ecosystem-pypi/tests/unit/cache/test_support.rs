use std::sync::Arc;

use bytes::Bytes;
use peryx_driver::download::DownloadHandle;
use peryx_driver::state::ServingState;
use peryx_storage::blob::Digest;

use super::{CacheError, CachedIndex};

pub fn persist_page(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    record: &CachedIndex,
) -> Result<(), CacheError> {
    super::fetch::persist_page_from(state, key, name, project, record, None)
}

pub fn flight_users(state: &ServingState, key: &str) -> usize {
    state.cache.inflight.active(key)
}

pub fn tail_download(
    state: Arc<ServingState>,
    digest: Digest,
    handle: DownloadHandle,
    route: String,
    filename: String,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
    super::download::tail_download(state, digest, handle, route, filename)
}

pub async fn settle_revalidation(state: &Arc<ServingState>, key: &str) {
    let guard = super::flight_gate(state, key).lock_owned().await;
    super::release_flight(state, key, guard);
}
