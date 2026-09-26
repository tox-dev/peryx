use std::collections::HashSet;
use std::num::NonZeroUsize;

use peryx_ha::{ArtifactPlacement, ArtifactSource, ByteAvailability, MAX_REPAIR_BATCH};
use tokio_util::sync::CancellationToken;

use crate::blob::{BlobError, BlobStorage};
use crate::meta::artifact_repair::{ArtifactRepairChange, ArtifactRepairDirection};
use crate::meta::{MetaError, MetaStore};

#[derive(Debug, thiserror::Error)]
pub enum ArtifactRepairError {
    #[error("repair batch must be between 1 and {MAX_REPAIR_BATCH}")]
    Batch,
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("artifact placement repair cancelled")]
    Cancelled,
    #[error("artifact content cursor was rejected and reset")]
    ContentCursorReset,
    #[error("artifact placement contains an invalid digest: {0}")]
    InvalidPlacementDigest(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactRepairDirectionReport {
    pub scanned: usize,
    pub changed: usize,
    pub skipped: usize,
    pub eof: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactRepairReport {
    pub content: ArtifactRepairDirectionReport,
    pub placement: ArtifactRepairDirectionReport,
}

#[derive(Debug)]
pub struct ArtifactRepairFailure {
    pub report: ArtifactRepairReport,
    pub error: ArtifactRepairError,
}

impl std::fmt::Display for ArtifactRepairFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for ArtifactRepairFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Reconciles one bounded content page and one bounded placement page.
///
/// # Errors
/// Returns a listing, presence, or metadata error without advancing the failing direction's cursor. A rejected content
/// cursor is reset and reported for the next invocation.
pub async fn repair_artifact_placements(
    meta: &MetaStore,
    blobs: &BlobStorage,
    batch: usize,
) -> Result<ArtifactRepairReport, ArtifactRepairError> {
    Box::pin(repair_artifact_placements_until(meta, blobs, batch, None))
        .await
        .map_err(|failure| failure.error)
}

/// Reconciles one bounded content page and one bounded placement page until cancellation.
///
/// # Errors
/// Returns a listing, presence, or metadata error without advancing the failing direction's cursor. A rejected content
/// cursor is reset and reported for the next invocation.
pub async fn repair_artifact_placements_cancellable(
    meta: &MetaStore,
    blobs: &BlobStorage,
    batch: usize,
    cancelled: &CancellationToken,
) -> Result<ArtifactRepairReport, Box<ArtifactRepairFailure>> {
    Box::pin(repair_artifact_placements_until(meta, blobs, batch, Some(cancelled))).await
}

async fn repair_artifact_placements_until(
    meta: &MetaStore,
    blobs: &BlobStorage,
    batch: usize,
    cancelled: Option<&CancellationToken>,
) -> Result<ArtifactRepairReport, Box<ArtifactRepairFailure>> {
    let mut report = ArtifactRepairReport::default();
    let Some(limit) = NonZeroUsize::new(batch).filter(|limit| limit.get() <= MAX_REPAIR_BATCH) else {
        return Err(Box::new(ArtifactRepairFailure {
            report,
            error: ArtifactRepairError::Batch,
        }));
    };
    let _ownership = match cancelled {
        Some(cancelled) => tokio::select! {
            () = cancelled.cancelled() => return Err(Box::new(ArtifactRepairFailure {
                report,
                error: ArtifactRepairError::Cancelled,
            })),
            ownership = meta.artifact_repair_ownership() => ownership,
        },
        None => meta.artifact_repair_ownership().await,
    };
    report.content = Box::pin(repair_content_page(meta, blobs, limit, cancelled))
        .await
        .map_err(|error| Box::new(ArtifactRepairFailure { report, error }))?;
    report.placement = repair_placement_page(meta, blobs, limit, cancelled)
        .await
        .map_err(|error| Box::new(ArtifactRepairFailure { report, error }))?;
    Ok(report)
}

async fn repair_content_page(
    meta: &MetaStore,
    blobs: &BlobStorage,
    limit: NonZeroUsize,
    cancelled: Option<&CancellationToken>,
) -> Result<ArtifactRepairDirectionReport, ArtifactRepairError> {
    let backend = blobs.repair_cursor_identity();
    let content_cursor = meta.artifact_repair_cursor_for_backend(ArtifactRepairDirection::Content, &backend)?;
    let content_page = match Box::pin(cancel_when(
        cancelled,
        blobs.digest_page(content_cursor.cursor(), limit),
    ))
    .await
    {
        Err(ArtifactRepairError::Blob(error)) if error.kind() == crate::blob::BlobErrorKind::InvalidCursor => {
            meta.apply_artifact_repair_page(ArtifactRepairDirection::Content, &content_cursor, None, &[])?;
            return Err(ArtifactRepairError::ContentCursorReset);
        }
        Ok(page) => page,
        Err(error) => return Err(error),
    };
    let content_digests = content_page
        .digests
        .iter()
        .map(|digest| digest.as_str().to_owned())
        .collect::<Vec<_>>();
    let content_observations = meta.artifact_repair_observations(&content_digests)?;
    let present = cancel_when(cancelled, blobs.present(content_page.digests))
        .await?
        .into_iter()
        .map(|digest| digest.as_str().to_owned())
        .collect::<HashSet<_>>();
    let content_updates = content_digests
        .iter()
        .filter_map(|digest| {
            if !present.contains(digest) {
                return None;
            }
            let observed = content_observations.get(digest)?.clone();
            let source = observed
                .placement
                .map_or(ArtifactSource::Unknown, |placement| placement.source);
            let replacement = ArtifactPlacement::record(source, true);
            (observed.placement != Some(replacement))
                .then(|| ArtifactRepairChange::promote(digest.clone(), observed, replacement))
        })
        .collect::<Vec<_>>();
    check_cancelled(cancelled)?;
    let (changed, skipped) = meta.apply_artifact_repair_page(
        ArtifactRepairDirection::Content,
        &content_cursor,
        content_page.next_cursor.clone(),
        &content_updates,
    )?;
    Ok(ArtifactRepairDirectionReport {
        scanned: content_digests.len(),
        changed,
        skipped,
        eof: content_page.next_cursor.is_none(),
    })
}

async fn repair_placement_page(
    meta: &MetaStore,
    blobs: &BlobStorage,
    limit: NonZeroUsize,
    cancelled: Option<&CancellationToken>,
) -> Result<ArtifactRepairDirectionReport, ArtifactRepairError> {
    let placement_cursor =
        meta.artifact_repair_cursor_for_backend(ArtifactRepairDirection::Placement, "placement/v1")?;
    let (placement_rows, placement_next_cursor) =
        meta.artifact_repair_placement_page(placement_cursor.cursor(), limit)?;
    let placement_digests = placement_rows
        .iter()
        .map(|(digest, _)| {
            crate::blob::Digest::from_hex(digest)
                .ok_or_else(|| ArtifactRepairError::InvalidPlacementDigest(digest.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let present = cancel_when(cancelled, blobs.present(placement_digests))
        .await?
        .into_iter()
        .map(|digest| digest.as_str().to_owned())
        .collect::<HashSet<_>>();
    let placement_updates = placement_rows
        .iter()
        .filter_map(|(digest, observed)| {
            let placement = observed.placement?;
            (!present.contains(digest) && placement.availability == ByteAvailability::Local).then(|| {
                ArtifactRepairChange::demote(
                    digest.clone(),
                    observed.clone(),
                    ArtifactPlacement::record(placement.source, false),
                )
            })
        })
        .collect::<Vec<_>>();
    check_cancelled(cancelled)?;
    let (changed, skipped) = meta.apply_artifact_repair_page(
        ArtifactRepairDirection::Placement,
        &placement_cursor,
        placement_next_cursor.clone(),
        &placement_updates,
    )?;
    Ok(ArtifactRepairDirectionReport {
        scanned: placement_rows.len(),
        changed,
        skipped,
        eof: placement_next_cursor.is_none(),
    })
}

fn check_cancelled(cancelled: Option<&CancellationToken>) -> Result<(), ArtifactRepairError> {
    cancelled
        .is_some_and(CancellationToken::is_cancelled)
        .then_some(ArtifactRepairError::Cancelled)
        .map_or(Ok(()), Err)
}

async fn cancel_when<T>(
    cancelled: Option<&CancellationToken>,
    operation: impl std::future::Future<Output = Result<T, BlobError>>,
) -> Result<T, ArtifactRepairError> {
    let Some(cancelled) = cancelled else {
        return Ok(operation.await?);
    };
    tokio::select! {
        () = cancelled.cancelled() => Err(ArtifactRepairError::Cancelled),
        result = operation => Ok(result?),
    }
}
