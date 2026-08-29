//! Streamed writes stay local until their digest is known. Commit uploads the verified stage under its
//! digest key with bounded concurrency and a durable multipart journal.

mod client;
mod config;

use std::collections::HashMap;
use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomicwrites::{AtomicFile, DisallowOverwrite};
use base64::Engine as _;
use bytes::Bytes;
use futures_util::stream::{BoxStream, FuturesUnordered};
use futures_util::{StreamExt as _, TryStreamExt as _};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Mutex, watch};

pub use self::client::{S3Client, S3Error};
use self::client::{S3Get, S3Part};
pub use self::config::{S3Addressing, S3Config, S3ConfigError, S3Settings};
use super::store::BlobStore;
use super::{
    BlobBackend, BlobCapabilities, BlobDurability, BlobError, BlobLease, BlobMetadata, BlobOperation, BlobRead,
    BlobReadBody, BlobStaged, BlobSupport, BlobWrite, Digest, DurabilityCapabilities, PlacementReceipt,
};

#[derive(Debug, Clone)]
pub struct S3Backend {
    client: S3Client,
    staging: BlobStore,
    acquisitions: Arc<Mutex<HashMap<PathBuf, Arc<UploadAcquisition>>>>,
}

#[derive(Debug)]
struct UploadAcquisition {
    result: watch::Receiver<Option<Result<String, String>>>,
}

#[derive(Clone, Copy)]
struct PartSlice {
    number: i32,
    offset: u64,
    bytes: u64,
}

impl S3Backend {
    #[must_use]
    pub fn new(config: S3Config, staging_dir: PathBuf) -> Self {
        Self {
            client: S3Client::new(config),
            staging: BlobStore::new(staging_dir),
            acquisitions: Arc::default(),
        }
    }

    #[must_use]
    pub const fn durability(&self) -> DurabilityCapabilities {
        self.client.config().durability()
    }

    fn key_for(&self, digest: &Digest) -> String {
        self.client.config().key_for(digest.as_str())
    }

    async fn open_inner(&self, digest: &Digest, range: Option<Range<u64>>) -> Result<BlobRead, BlobError> {
        let key = self.key_for(digest);
        let head = if range.is_some() {
            Some(
                self.client
                    .head(&key)
                    .await
                    .map_err(|error| blob_error(error, Some(digest)))?
                    .ok_or_else(|| BlobError::not_found(digest))?,
            )
        } else {
            None
        };
        let total = head.as_ref().map_or(0, |head| head.bytes);
        if let Some(range) = &range
            && (range.start > range.end || range.end > total)
        {
            return Err(BlobError::invalid_range(range.start, range.end, total));
        }
        if let Some(range) = &range
            && range.is_empty()
        {
            return Ok(BlobRead::new(
                "s3",
                digest.clone(),
                BlobMetadata {
                    bytes: total,
                    modified: None,
                },
                range.clone(),
                BlobReadBody::Stream(futures_util::stream::empty().boxed()),
            ));
        }
        let if_match = head
            .as_ref()
            .map(|head| {
                head.etag
                    .as_deref()
                    .ok_or_else(|| blob_error(S3Error::InvalidResponse("ETag"), Some(digest)))
            })
            .transpose()?;
        let response = self
            .client
            .get(&key, range.clone(), if_match)
            .await
            .map_err(|error| blob_error(error, Some(digest)))?;
        if head.as_ref().is_some_and(|head| head.bytes != response.total_bytes) {
            return Err(blob_error(
                S3Error::InvalidResponse("content range total"),
                Some(digest),
            ));
        }
        let total = response.total_bytes;
        let range = range.unwrap_or(0..total);
        Ok(BlobRead::new(
            "s3",
            digest.clone(),
            BlobMetadata {
                bytes: total,
                modified: None,
            },
            range,
            BlobReadBody::Stream(stream_body(response)),
        ))
    }

    async fn head_inner(&self, digest: &Digest) -> Result<Option<BlobMetadata>, BlobError> {
        Ok(self
            .client
            .head(&self.key_for(digest))
            .await
            .map_err(|error| blob_error(error, Some(digest)))?
            .map(|head| BlobMetadata {
                bytes: head.bytes,
                modified: None,
            }))
    }

    async fn verify_inner(&self, digest: &Digest) -> Result<bool, BlobError> {
        let response = match self.client.get(&self.key_for(digest), None, None).await {
            Ok(response) => response,
            Err(S3Error::NotFound) => return Err(BlobError::not_found(digest)),
            Err(error) => return Err(blob_error(error, Some(digest))),
        };
        let mut hasher = Sha256::new();
        let mut body = stream_body(response);
        while let Some(chunk) = body.try_next().await? {
            hasher.update(&chunk);
        }
        Ok(hex::encode(hasher.finalize()) == digest.as_str())
    }

    async fn delete_inner(&self, digest: &Digest) -> Result<bool, BlobError> {
        let key = self.key_for(digest);
        let existed = self
            .client
            .head(&key)
            .await
            .map_err(|error| blob_error(error, Some(digest)))?
            .is_some();
        self.client
            .delete(&key)
            .await
            .map_err(|error| blob_error(error, Some(digest)))?;
        Ok(existed)
    }

    async fn materialize_inner(&self, digest: &Digest) -> Result<BlobLease, BlobError> {
        let response = match self.client.get(&self.key_for(digest), None, None).await {
            Ok(response) => response,
            Err(S3Error::NotFound) => return Err(BlobError::not_found(digest)),
            Err(error) => return Err(blob_error(error, Some(digest))),
        };
        let dir = self.staging.staging_dir();
        std::fs::create_dir_all(&dir).map_err(BlobError::from)?;
        let (file, temp_path) = tempfile::Builder::new()
            .prefix(".peryx-s3-")
            .tempfile_in(&dir)
            .map_err(BlobError::from)?
            .into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let mut body = stream_body(response);
        while let Some(chunk) = body.try_next().await? {
            file.write_all(&chunk).await.map_err(BlobError::from)?;
        }
        file.flush().await.map_err(BlobError::from)?;
        Ok(BlobLease::downloaded(temp_path))
    }

    async fn upload(&self, staged: &BlobStaged) -> Result<(), BlobError> {
        let digest = staged.digest().clone();
        let len = staged.len();
        let path = staged.with_materialized(Path::to_path_buf);
        let key = self.key_for(&digest);
        let result = if len <= self.client.config().multipart_threshold {
            self.put_whole(&key, &digest, &path).await
        } else {
            self.put_multipart(&key, &digest, len, &path).await
        };
        match result {
            Ok(()) | Err(S3Error::AlreadyExists) => Ok(()),
            Err(error) => Err(error),
        }
        .map_err(|error| blob_error(error, Some(&digest)).with_context("s3", BlobOperation::Commit, Some(&digest)))
    }

    async fn put_whole(&self, key: &str, digest: &Digest, path: &Path) -> Result<(), S3Error> {
        let checksum = digest_checksum(digest);
        let mut conflicts = 0;
        loop {
            match self.client.put_file(key, path, &checksum).await {
                Err(S3Error::Conflict) if conflicts < self.client.config().max_retries => conflicts += 1,
                result => return result,
            }
        }
    }

    async fn put_multipart(&self, key: &str, digest: &Digest, len: u64, path: &Path) -> Result<(), S3Error> {
        let part_size = self.client.config().part_size.max(len.div_ceil(10_000));
        let journal = self.multipart_journal(digest);
        let mut conflicts = 0;
        let mut recovered_stale_upload = false;
        loop {
            let upload_id = self.acquire_upload(key, &journal).await?;
            let parts = match self.upload_parts(key, &upload_id, len, part_size, path).await {
                Ok(parts) => parts,
                Err(S3Error::NoSuchUpload) if !recovered_stale_upload => {
                    recovered_stale_upload = true;
                    remove_journal(&journal).await?;
                    continue;
                }
                Err(error) => return self.abort_after_error(key, &upload_id, &journal, error).await,
            };
            match self.client.complete_multipart(key, &upload_id, parts).await {
                Ok(()) => return remove_journal(&journal).await,
                Err(S3Error::AlreadyExists) => return self.abort_and_clear(key, &upload_id, &journal).await,
                Err(S3Error::Conflict) if conflicts < self.client.config().max_retries => {
                    conflicts += 1;
                    self.abort_and_clear(key, &upload_id, &journal).await?;
                }
                Err(S3Error::NoSuchUpload) => match self.client.head(key).await {
                    Ok(Some(_)) => return remove_journal(&journal).await,
                    Ok(None) if !recovered_stale_upload => {
                        recovered_stale_upload = true;
                        remove_journal(&journal).await?;
                    }
                    Ok(None) => {
                        return self
                            .abort_after_error(key, &upload_id, &journal, S3Error::NoSuchUpload)
                            .await;
                    }
                    Err(error) => return self.abort_after_error(key, &upload_id, &journal, error).await,
                },
                Err(error) => return self.abort_after_error(key, &upload_id, &journal, error).await,
            }
        }
    }

    fn multipart_journal(&self, digest: &Digest) -> PathBuf {
        self.staging.staging_dir().join("s3-multipart").join(digest.as_str())
    }

    async fn acquire_upload(&self, key: &str, journal: &Path) -> Result<String, S3Error> {
        if let Some(upload_id) = read_journal(journal).await? {
            return Ok(upload_id);
        }
        let journal = journal.to_owned();
        let (acquisition, publisher) = {
            let mut acquisitions = self.acquisitions.lock().await;
            let existing = acquisitions.get(&journal).cloned();
            let result = existing.map_or_else(
                || {
                    let (publisher, result) = watch::channel(None);
                    let acquisition = Arc::new(UploadAcquisition { result });
                    acquisitions.insert(journal.clone(), Arc::clone(&acquisition));
                    (acquisition, Some(publisher))
                },
                |acquisition| (acquisition, None),
            );
            drop(acquisitions);
            result
        };
        if let Some(publisher) = publisher {
            let client = self.client.clone();
            let key = key.to_owned();
            let acquisitions = Arc::clone(&self.acquisitions);
            tokio::spawn(async move {
                let result = create_upload(&client, &key, &journal)
                    .await
                    .map_err(|error| error.to_string());
                publisher.send_replace(Some(result));
                acquisitions.lock().await.remove(&journal);
            });
        }
        let mut result = acquisition.result.clone();
        loop {
            let available = result.borrow().clone();
            if let Some(outcome) = available {
                return outcome.map_err(S3Error::Request);
            }
            result
                .changed()
                .await
                .expect("multipart upload acquisition publishes a result before closing");
        }
    }

    async fn abort_after_error(
        &self,
        key: &str,
        upload_id: &str,
        journal: &Path,
        error: S3Error,
    ) -> Result<(), S3Error> {
        match self.abort_and_clear(key, upload_id, journal).await {
            Ok(()) => Err(error),
            Err(abort) => Err(S3Error::Request(format!(
                "{error}; abort of multipart upload {upload_id} failed: {abort}"
            ))),
        }
    }

    async fn abort_and_clear(&self, key: &str, upload_id: &str, journal: &Path) -> Result<(), S3Error> {
        self.client.abort_multipart(key, upload_id).await?;
        remove_journal(journal).await
    }

    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        len: u64,
        part_size: u64,
        path: &Path,
    ) -> Result<Vec<S3Part>, S3Error> {
        let part_size = usize::try_from(part_size).expect("validated S3 part size fits usize");
        let part_bytes = u64::try_from(part_size).expect("part size originated as u64");
        let mut pending = (0..len)
            .step_by(part_size)
            .enumerate()
            .map(|(index, offset)| PartSlice {
                number: i32::try_from(index + 1).expect("multipart part count is bounded to 10,000"),
                offset,
                bytes: part_bytes.min(len - offset),
            });
        let mut uploads = FuturesUnordered::new();
        for _ in 0..self.client.config().upload_concurrency {
            let Some(part) = pending.next() else {
                break;
            };
            uploads.push(self.upload_one(key, upload_id, path, part));
        }
        let mut parts = Vec::new();
        let mut first_error = None;
        while let Some(result) = uploads.next().await {
            match result {
                Ok(part) => parts.push(part),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
            if first_error.is_none()
                && let Some(part) = pending.next()
            {
                uploads.push(self.upload_one(key, upload_id, path, part));
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        parts.sort_unstable_by_key(|part| part.number);
        Ok(parts)
    }

    async fn upload_one(&self, key: &str, upload_id: &str, path: &Path, part: PartSlice) -> Result<S3Part, S3Error> {
        self.client
            .upload_part(key, upload_id, part.number, path, part.offset, part.bytes)
            .await
    }
}

async fn create_upload(client: &S3Client, key: &str, journal: &Path) -> Result<String, S3Error> {
    let upload_id = client.create_multipart(key).await?;
    match create_journal(journal, &upload_id).await {
        Ok(()) => Ok(upload_id),
        Err(error) => {
            client.abort_multipart(key, &upload_id).await?;
            read_journal(journal)
                .await?
                .ok_or_else(|| S3Error::Request(error.to_string()))
        }
    }
}

async fn read_journal(path: &Path) -> Result<Option<String>, S3Error> {
    match tokio::fs::read(path).await {
        Ok(bytes) if bytes.is_empty() || bytes.len() > 4_096 => remove_journal(path).await.map(|()| None),
        Ok(bytes) => {
            if let Ok(upload_id) = String::from_utf8(bytes) {
                Ok(Some(upload_id))
            } else {
                remove_journal(path).await.map(|()| None)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(S3Error::Request(error.to_string())),
    }
}

async fn create_journal(path: &Path, upload_id: &str) -> Result<(), std::io::Error> {
    let parent = path.parent().expect("multipart journal path has a parent");
    tokio::fs::create_dir_all(parent).await?;
    let path = path.to_owned();
    let upload_id = upload_id.to_owned();
    tokio::task::spawn_blocking(move || {
        AtomicFile::new(&path, DisallowOverwrite)
            .write(|file| file.write_all(upload_id.as_bytes()))
            .map_err(std::io::Error::from)?;
        tracing::debug!(target: "peryx_storage::s3_journal", path = %path.display(), "persisted multipart upload journal");
        Ok(())
    })
    .await
    .map_err(std::io::Error::other)?
}

async fn remove_journal(path: &Path) -> Result<(), S3Error> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(S3Error::Request(error.to_string())),
    }
}

fn digest_checksum(digest: &Digest) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(hex::decode(digest.as_str()).expect("digest contains validated lowercase hex"))
}

/// Sharing this non-capturing mapper avoids a stream-error monomorphization per caller.
fn stream_body(response: S3Get) -> BoxStream<'static, Result<Bytes, BlobError>> {
    response.body.map_err(BlobError::from).boxed()
}

impl From<S3Error> for BlobError {
    fn from(error: S3Error) -> Self {
        blob_error(error, None)
    }
}

fn blob_error(error: S3Error, digest: Option<&Digest>) -> BlobError {
    match error {
        S3Error::NotFound => digest.map_or_else(
            || BlobError::io(std::io::Error::from(std::io::ErrorKind::NotFound)),
            BlobError::not_found,
        ),
        other => BlobError::io(std::io::Error::other(other.to_string())),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/blob/s3/tests.rs"]
mod tests;

impl BlobBackend for S3Backend {
    fn capabilities(&self) -> BlobCapabilities {
        BlobCapabilities {
            durability: BlobDurability::ObjectStore,
            create_if_absent: if self.client.config().conditional_writes {
                BlobSupport::Native
            } else {
                BlobSupport::Unsupported
            },
            range: BlobSupport::Native,
            checksum: BlobSupport::Emulated,
            delete: BlobSupport::Native,
            list: BlobSupport::Unsupported,
            local_tail: BlobSupport::Native,
        }
    }

    async fn health(&self) -> Result<(), BlobError> {
        self.client
            .health()
            .await
            .map_err(|error| blob_error(error, None).with_context("s3", BlobOperation::Health, None))
    }

    async fn open(&self, digest: Digest, range: Option<Range<u64>>) -> Result<BlobRead, BlobError> {
        self.open_inner(&digest, range)
            .await
            .map_err(|error| error.with_context("s3", BlobOperation::Open, Some(&digest)))
    }

    async fn head(&self, digest: Digest) -> Result<Option<BlobMetadata>, BlobError> {
        self.head_inner(&digest)
            .await
            .map_err(|error| error.with_context("s3", BlobOperation::Head, Some(&digest)))
    }

    async fn begin(&self) -> Result<BlobWrite, BlobError> {
        let inner = BlobWrite::filesystem(self.staging.clone())
            .map_err(|error| error.with_context("s3", BlobOperation::Write, None))?;
        Ok(BlobWrite::s3(S3Write {
            inner: Box::new(inner),
            backend: self.clone(),
        }))
    }

    async fn verify(&self, digest: Digest) -> Result<bool, BlobError> {
        self.verify_inner(&digest)
            .await
            .map_err(|error| error.with_context("s3", BlobOperation::Verify, Some(&digest)))
    }

    async fn delete(&self, digest: Digest) -> Result<bool, BlobError> {
        self.delete_inner(&digest)
            .await
            .map_err(|error| error.with_context("s3", BlobOperation::Delete, Some(&digest)))
    }

    async fn materialize(&self, digest: Digest) -> Result<BlobLease, BlobError> {
        self.materialize_inner(&digest)
            .await
            .map_err(|error| error.with_context("s3", BlobOperation::Materialize, Some(&digest)))
    }
}

pub struct S3Write {
    inner: Box<BlobWrite>,
    backend: S3Backend,
}

impl S3Write {
    pub(crate) async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), BlobError> {
        // Boxing terminates the recursive future type through `BlobWrite`.
        Box::pin(self.inner.write_chunk(chunk)).await
    }

    pub(crate) async fn flush(&mut self) -> Result<u64, BlobError> {
        Box::pin(self.inner.flush()).await
    }

    pub(crate) fn tail(&self) -> Option<super::BlobTail> {
        self.inner.tail()
    }

    pub(crate) async fn finish(self) -> Result<BlobStaged, BlobError> {
        Ok(BlobStaged::s3(S3Staged {
            inner: Box::new(Box::pin(self.inner.finish()).await?),
            backend: self.backend,
        }))
    }

    pub(crate) async fn commit(self, expected: &Digest) -> Result<PlacementReceipt, BlobError> {
        self.finish().await?.commit_as(expected).await
    }

    pub(crate) async fn abort(self) -> Result<(), BlobError> {
        Box::pin(self.inner.abort()).await
    }
}

#[derive(Debug)]
pub struct S3Staged {
    inner: Box<BlobStaged>,
    backend: S3Backend,
}

impl S3Staged {
    pub(crate) const fn digest(&self) -> &Digest {
        self.inner.digest()
    }

    pub(crate) const fn len(&self) -> u64 {
        self.inner.len()
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// A non-generic callback avoids monomorphizing callers that inspect filesystem stages.
    pub(crate) const fn inner(&self) -> &BlobStaged {
        &self.inner
    }

    pub(crate) async fn commit(self) -> Result<PlacementReceipt, BlobError> {
        self.backend.upload(&self.inner).await?;
        let receipt = PlacementReceipt {
            digest: self.inner.digest().clone(),
            size: self.inner.len(),
            durability: self.backend.durability(),
        };
        Box::pin(self.inner.abort()).await?;
        Ok(receipt)
    }

    pub(crate) async fn abort(self) -> Result<(), BlobError> {
        Box::pin(self.inner.abort()).await
    }
}
