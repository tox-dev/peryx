//! Serving hosted and upstream PEP 740 provenance objects.

use std::sync::Arc;

use crate::policy::{PypiPolicy as _, RemoteMetadataMode};
use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt as _, StreamExt as _, TryFutureExt as _, TryStreamExt as _};
use peryx_driver::state::ServingState;
use peryx_index::{Index, IndexKind};
use peryx_storage::blob::Digest;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use url::Url;

use crate::store::PypiStore as _;
use crate::store::{AttestationAvailability, UpstreamAttestation};

use super::{
    CacheError, ensure_digest_clear, flight_gate, freshness_secs, metadata_upstream_permit, release_flight,
    source_client,
};

const MAX_PROVENANCE_BYTES: usize = 2 * 1024 * 1024;

/// A hosted or upstream provenance representation and the state exposed with it.
pub struct ProvenanceBody {
    /// The structurally accepted, unverified PEP 740 document.
    pub bytes: Bytes,
    /// The accepted upstream media type, or the PEP 740 media type for hosted content.
    pub media_type: String,
    /// `hosted`, the cached index, or the routed upstream that supplied the document.
    pub source: String,
    /// Whether the URL is immutable because the document belongs to a hosted artifact.
    pub immutable: bool,
    /// Whether the body remains only at its source or is retained locally.
    pub availability: AttestationAvailability,
}

/// # Errors
/// Returns a store, blob, policy, validation, or upstream error when no representation can be served.
pub async fn provenance_bytes(
    state: &Arc<ServingState>,
    index: &Index,
    artifact_digest: &Digest,
    filename: &str,
) -> Result<ProvenanceBody, CacheError> {
    ensure_digest_clear(state, artifact_digest)?;
    find_attestation_source(state, index, artifact_digest.as_str(), filename)
        .and_then(|source| async move {
            let Some(source) = source else {
                return Err(CacheError::FileNotFound);
            };
            let (source, record) = match source {
                AttestationSource::Hosted(provenance_hex) => return hosted_provenance(state, &provenance_hex).await,
                AttestationSource::Upstream { source, record } => (source, record),
            };
            match index.policy.remote_metadata_mode() {
                RemoteMetadataMode::Direct => Err(CacheError::FileNotFound),
                RemoteMetadataMode::Proxy => {
                    fetch_upstream_attestation(state, &record, false)
                        .await
                        .and_then(|outcome| match outcome {
                            FetchOutcome::Body(fetched) => Ok(fetched.into_body(reported_source(&record))),
                            FetchOutcome::NotModified { .. } | FetchOutcome::DiscardValidators => {
                                Err(CacheError::Unavailable)
                            }
                        })
                }
                RemoteMetadataMode::Cache => {
                    cached_upstream_attestation(state, &source, artifact_digest.as_str(), filename, *record).await
                }
            }
        })
        .await
}

enum AttestationSource {
    Hosted(String),
    Upstream {
        source: String,
        record: Box<UpstreamAttestation>,
    },
}

fn find_attestation_source<'a>(
    state: &'a Arc<ServingState>,
    index: &'a Index,
    artifact_sha256: &'a str,
    filename: &'a str,
) -> BoxFuture<'a, Result<Option<AttestationSource>, CacheError>> {
    async move {
        match &index.kind {
            IndexKind::Hosted { .. } => {
                let project = artifact_project(filename);
                layer_has_attestation(state, index, &project, artifact_sha256, filename)
                    .await
                    .and_then(|visible| {
                        if !visible {
                            return Ok(None);
                        }
                        state
                            .meta
                            .get_provenance(&index.name, &project, artifact_sha256, filename)
                            .map_err(CacheError::from)
                            .map(|record| record.map(|(digest, _size)| AttestationSource::Hosted(digest)))
                    })
            }
            IndexKind::Cached { .. } => {
                let project = artifact_project(filename);
                layer_has_attestation(state, index, &project, artifact_sha256, filename)
                    .await
                    .and_then(|visible| {
                        if !visible {
                            return Ok(None);
                        }
                        state
                            .meta
                            .get_upstream_attestation(&index.name, &project, artifact_sha256, filename)
                            .map_err(CacheError::from)
                            .map(|record| {
                                record.map(|record| AttestationSource::Upstream {
                                    source: index.name.clone(),
                                    record: Box::new(record),
                                })
                            })
                    })
            }
            IndexKind::Virtual { layers, .. } => {
                layer_has_attestation(state, index, &artifact_project(filename), artifact_sha256, filename)
                    .and_then(|visible| async move {
                        if !visible {
                            return Ok(None);
                        }
                        futures_util::stream::iter(peryx_index::shadow_order(&state.indexes, layers))
                            .then(|position| {
                                find_attestation_source(state, state.index_at(position), artifact_sha256, filename)
                            })
                            .try_filter_map(|source| futures_util::future::ready(Ok::<_, CacheError>(source)))
                            .try_next()
                            .await
                    })
                    .await
            }
        }
    }
    .boxed()
}

fn artifact_project(filename: &str) -> String {
    crate::parse_distribution_filename(filename).map_or_else(
        |_| crate::project_of_filename(filename),
        |parsed| parsed.normalized_name,
    )
}

async fn layer_has_attestation(
    state: &Arc<ServingState>,
    index: &Index,
    project: &str,
    artifact_sha256: &str,
    filename: &str,
) -> Result<bool, CacheError> {
    let detail = match &index.kind {
        IndexKind::Cached { .. } => state
            .meta
            .get_index(&format!("{}/{project}", index.name))
            .map_err(CacheError::from)
            .and_then(|record| {
                record
                    .map(|record| crate::cache::resolve::raw_to_detail(state, &index.route, &record))
                    .transpose()
            }),
        IndexKind::Hosted { .. } | IndexKind::Virtual { .. } => {
            crate::cache::resolve::resolve_detail_optional(state, index, project, &index.route).await
        }
    };
    detail.map(|detail| {
        detail.is_some_and(|detail| {
            detail.files.into_iter().any(|file| {
                file.filename == filename
                    && file.sha256() == Some(artifact_sha256)
                    && matches!(file.provenance, crate::simple::Provenance::Url(_))
            })
        })
    })
}

async fn hosted_provenance(state: &Arc<ServingState>, provenance_hex: &str) -> Result<ProvenanceBody, CacheError> {
    futures_util::future::ready(Digest::from_hex(provenance_hex).ok_or(CacheError::FileNotFound))
        .and_then(|digest| async move {
            state
                .blobs
                .head(&digest)
                .await
                .map_err(CacheError::from)
                .and_then(|head| head.map_or(Err(CacheError::FileNotFound), |_| Ok(digest)))
        })
        .and_then(|digest| async move {
            state
                .blobs
                .read_bytes(&digest, MAX_PROVENANCE_BYTES as u64)
                .await
                .map_err(CacheError::from)
                .map(|bytes| ProvenanceBody {
                    bytes: Bytes::from(bytes),
                    media_type: crate::attestation::PROVENANCE_MEDIA_TYPE.to_owned(),
                    source: "hosted".to_owned(),
                    immutable: true,
                    availability: AttestationAvailability::Cached,
                })
        })
        .await
}

async fn cached_upstream_attestation(
    state: &Arc<ServingState>,
    source: &str,
    artifact_sha256: &str,
    filename: &str,
    record: UpstreamAttestation,
) -> Result<ProvenanceBody, CacheError> {
    if attestation_fresh(state, &record) {
        return retained_body(record);
    }
    let key = format!(
        "attestation\0{source}\0{}\0{artifact_sha256}\0{filename}",
        record.project
    );
    let guard = flight_gate(state, &key).lock_owned().await;
    let outcome = refresh_cached_attestation(state, source, &record.project, artifact_sha256, filename).await;
    release_flight(state, &key, guard);
    outcome
}

fn refresh_cached_attestation<'a>(
    state: &'a Arc<ServingState>,
    source: &'a str,
    project: &'a str,
    digest: &'a str,
    filename: &'a str,
) -> BoxFuture<'a, Result<ProvenanceBody, CacheError>> {
    refresh_cached_attestation_attempt(
        RefreshContext {
            state,
            source,
            project,
            digest,
            filename,
        },
        RefreshAttempt {
            retry_conflict: true,
            refetched_without_validators: false,
        },
    )
}

#[derive(Clone, Copy)]
struct RefreshContext<'a> {
    state: &'a Arc<ServingState>,
    source: &'a str,
    project: &'a str,
    digest: &'a str,
    filename: &'a str,
}

#[derive(Clone, Copy)]
struct RefreshAttempt {
    retry_conflict: bool,
    refetched_without_validators: bool,
}

fn refresh_cached_attestation_attempt(
    context: RefreshContext<'_>,
    attempt: RefreshAttempt,
) -> BoxFuture<'_, Result<ProvenanceBody, CacheError>> {
    async move {
        futures_util::future::ready(
            context
                .state
                .meta
                .get_upstream_attestation(context.source, context.project, context.digest, context.filename)
                .map_err(CacheError::from)
                .and_then(|record| record.ok_or(CacheError::FileNotFound)),
        )
        .and_then(|mut record| async move {
            if attestation_fresh(context.state, &record) {
                return retained_body(record);
            }
            let expected = record.clone();
            match fetch_upstream_attestation(context.state, &record, record.body.is_some()).await {
                Ok(FetchOutcome::NotModified {
                    fresh_secs,
                    must_revalidate,
                }) => {
                    record.fetched_at_unix = Some((context.state.clock)());
                    record.fresh_secs = fresh_secs.or(record.fresh_secs);
                    if let Some(must_revalidate) = must_revalidate {
                        record.must_revalidate = must_revalidate;
                    }
                    commit_refreshed_attestation(context, attempt, expected, record).await
                }
                Ok(FetchOutcome::DiscardValidators) => discard_validators(context, attempt, expected, &record).await,
                Ok(FetchOutcome::Body(fetched)) if !fetched.storable => {
                    serve_unstored_attestation(context, attempt, expected, &record, fetched).await
                }
                Ok(FetchOutcome::Body(fetched)) => {
                    record.media_type = Some(fetched.media_type);
                    record.etag = fetched.etag;
                    record.last_modified = fetched.last_modified;
                    record.fetched_at_unix = Some((context.state.clock)());
                    record.fresh_secs = fetched.fresh_secs;
                    record.must_revalidate = fetched.must_revalidate;
                    record.availability = AttestationAvailability::Cached;
                    record.body = Some(
                        std::str::from_utf8(&fetched.bytes)
                            .expect("a parsed JSON document is UTF-8")
                            .to_owned(),
                    );
                    commit_refreshed_attestation(context, attempt, expected, record).await
                }
                Err(err)
                    if transient_upstream_failure(&err) && attestation_stale_serviceable(context.state, &record) =>
                {
                    retained_body(record)
                }
                Err(err) => Err(err),
            }
        })
        .await
    }
    .boxed()
}

fn discard_validators<'a>(
    context: RefreshContext<'a>,
    attempt: RefreshAttempt,
    expected: UpstreamAttestation,
    record: &UpstreamAttestation,
) -> BoxFuture<'a, Result<ProvenanceBody, CacheError>> {
    if attempt.refetched_without_validators {
        return futures_util::future::ready(Err(CacheError::Unavailable)).boxed();
    }
    let remote = remote_attestation(record);
    async move {
        futures_util::future::ready(compare_attestation(context, &expected, &remote))
            .and_then(|replaced| async move {
                if replaced {
                    refresh_cached_attestation_attempt(
                        context,
                        RefreshAttempt {
                            refetched_without_validators: true,
                            ..attempt
                        },
                    )
                    .await
                } else {
                    retry_attestation(context, attempt).await
                }
            })
            .await
    }
    .boxed()
}

fn serve_unstored_attestation<'a>(
    context: RefreshContext<'a>,
    attempt: RefreshAttempt,
    expected: UpstreamAttestation,
    record: &UpstreamAttestation,
    fetched: FetchedBody,
) -> BoxFuture<'a, Result<ProvenanceBody, CacheError>> {
    let remote = remote_attestation(record);
    async move {
        futures_util::future::ready(compare_attestation(context, &expected, &remote))
            .and_then(|replaced| async move {
                if replaced {
                    Ok(fetched.into_body(reported_source(&remote)))
                } else {
                    retry_attestation(context, attempt).await
                }
            })
            .await
    }
    .boxed()
}

fn remote_attestation(record: &UpstreamAttestation) -> UpstreamAttestation {
    UpstreamAttestation::remote(&record.url, &record.source, &record.project, record.upstream.as_deref())
}

fn compare_attestation(
    context: RefreshContext<'_>,
    expected: &UpstreamAttestation,
    record: &UpstreamAttestation,
) -> Result<bool, CacheError> {
    context
        .state
        .meta
        .compare_exchange_upstream_attestation(context.source, context.digest, context.filename, expected, record)
        .map_err(CacheError::from)
}

fn commit_refreshed_attestation(
    context: RefreshContext<'_>,
    attempt: RefreshAttempt,
    expected: UpstreamAttestation,
    record: UpstreamAttestation,
) -> BoxFuture<'_, Result<ProvenanceBody, CacheError>> {
    async move {
        futures_util::future::ready(compare_attestation(context, &expected, &record))
            .and_then(|replaced| async move {
                if replaced {
                    retained_body(record)
                } else {
                    retry_attestation(context, attempt).await
                }
            })
            .await
    }
    .boxed()
}

fn retry_attestation(
    context: RefreshContext<'_>,
    attempt: RefreshAttempt,
) -> BoxFuture<'_, Result<ProvenanceBody, CacheError>> {
    if !attempt.retry_conflict {
        return futures_util::future::ready(Err(CacheError::Unavailable)).boxed();
    }
    refresh_cached_attestation_attempt(
        context,
        RefreshAttempt {
            retry_conflict: false,
            ..attempt
        },
    )
}

fn retained_body(record: UpstreamAttestation) -> Result<ProvenanceBody, CacheError> {
    let source = reported_source(&record);
    record.body.ok_or(CacheError::Unavailable).map(|body| ProvenanceBody {
        bytes: Bytes::from(body),
        media_type: record
            .media_type
            .unwrap_or_else(|| crate::attestation::PROVENANCE_MEDIA_TYPE.to_owned()),
        source,
        immutable: false,
        availability: AttestationAvailability::Cached,
    })
}

fn attestation_fresh(state: &ServingState, record: &UpstreamAttestation) -> bool {
    record.body.is_some()
        && record.fetched_at_unix.is_some_and(|fetched_at| {
            (state.clock)().saturating_sub(fetched_at) < freshness_secs(state.ttl_secs, record.fresh_secs)
        })
}

fn attestation_stale_serviceable(state: &ServingState, record: &UpstreamAttestation) -> bool {
    !record.must_revalidate
        && record.body.is_some()
        && record.fetched_at_unix.is_some_and(|fetched_at| {
            peryx_index::serving::within_stale_bound(
                (state.clock)(),
                state.max_stale_secs,
                fetched_at,
                freshness_secs(state.ttl_secs, record.fresh_secs),
            )
        })
}

fn transient_upstream_failure(err: &CacheError) -> bool {
    match err {
        CacheError::Upstream(peryx_upstream::UpstreamError::Http(err)) => err.status().is_none_or(|status| {
            status.is_server_error() || matches!(status, StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS)
        }),
        CacheError::Unavailable
        | CacheError::OfflineMissing(_)
        | CacheError::RateLimited { .. }
        | CacheError::UpstreamRateLimited { .. } => true,
        _ => false,
    }
}

enum FetchOutcome {
    NotModified {
        fresh_secs: Option<i64>,
        must_revalidate: Option<bool>,
    },
    DiscardValidators,
    Body(FetchedBody),
}

struct FetchedBody {
    bytes: Bytes,
    media_type: String,
    etag: Option<String>,
    last_modified: Option<String>,
    fresh_secs: Option<i64>,
    must_revalidate: bool,
    storable: bool,
}

impl FetchedBody {
    fn into_body(self, source: String) -> ProvenanceBody {
        ProvenanceBody {
            bytes: self.bytes,
            media_type: self.media_type,
            source,
            immutable: false,
            availability: AttestationAvailability::RemoteOnly,
        }
    }
}

fn reported_source(record: &UpstreamAttestation) -> String {
    record.upstream.clone().unwrap_or_else(|| record.source.clone())
}

async fn fetch_upstream_attestation(
    state: &ServingState,
    record: &UpstreamAttestation,
    conditional: bool,
) -> Result<FetchOutcome, CacheError> {
    let (etag, last_modified) = if conditional {
        (record.etag.as_deref(), record.last_modified.as_deref())
    } else {
        (None, None)
    };
    futures_util::future::ready(source_client(state, &record.source, record.upstream.as_deref()))
        .and_then(|(client, offline)| async move {
            if offline {
                return Err(CacheError::OfflineMissing("provenance"));
            }
            metadata_upstream_permit(state, &record.source)
                .and_then(|permit| async move {
                    futures_util::future::ready(
                        Url::parse(&record.url)
                            .map_err(peryx_upstream::UpstreamError::from)
                            .map_err(CacheError::from),
                    )
                    .and_then(|url| async move {
                        client
                            .send_validated(
                                url,
                                "application/vnd.pypi.integrity.v1+json, application/json;q=0.9",
                                etag,
                                last_modified,
                            )
                            .map_err(CacheError::from)
                            .and_then(|response| async move {
                                let _permit = permit;
                                process_upstream_attestation_response(response).await
                            })
                            .await
                    })
                    .await
                })
                .await
        })
        .await
}

async fn process_upstream_attestation_response(response: reqwest::Response) -> Result<FetchOutcome, CacheError> {
    if response.status() == StatusCode::NOT_MODIFIED {
        let cache = crate::simple_client::response_cache_policy(response.headers());
        if cache.storable {
            return Ok(FetchOutcome::NotModified {
                fresh_secs: cache.fresh_secs,
                must_revalidate: cache.must_revalidate,
            });
        }
        return Ok(FetchOutcome::DiscardValidators);
    }
    if response.status() == StatusCode::NOT_FOUND {
        return Err(CacheError::FileNotFound);
    }
    futures_util::future::ready(
        response
            .error_for_status()
            .map_err(peryx_upstream::UpstreamError::from)
            .map_err(CacheError::from),
    )
    .and_then(|response| async move {
        futures_util::future::ready(provenance_media_type(
            response.url(),
            response.headers().get(CONTENT_TYPE),
        ))
        .and_then(|media_type| async move {
            if response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > MAX_PROVENANCE_BYTES)
            {
                return Err(peryx_upstream::UpstreamError::ResponseTooLarge {
                    limit: MAX_PROVENANCE_BYTES,
                }
                .into());
            }
            let etag = header_string(response.headers().get(ETAG));
            let last_modified = header_string(response.headers().get(LAST_MODIFIED));
            let cache = crate::simple_client::response_cache_policy(response.headers());
            response
                .bytes_stream()
                .map_err(peryx_upstream::UpstreamError::from)
                .map_err(CacheError::from)
                .try_fold(Vec::new(), |mut bytes, chunk| async move {
                    if bytes
                        .len()
                        .checked_add(chunk.len())
                        .is_none_or(|length| length > MAX_PROVENANCE_BYTES)
                    {
                        return Err(peryx_upstream::UpstreamError::ResponseTooLarge {
                            limit: MAX_PROVENANCE_BYTES,
                        }
                        .into());
                    }
                    bytes.extend_from_slice(&chunk);
                    Ok(bytes)
                })
                .await
                .and_then(|bytes| {
                    validate_provenance(&bytes).map(|()| {
                        FetchOutcome::Body(FetchedBody {
                            bytes: Bytes::from(bytes),
                            media_type,
                            etag,
                            last_modified,
                            fresh_secs: cache.fresh_secs,
                            must_revalidate: cache.must_revalidate.unwrap_or(false),
                            storable: cache.storable,
                        })
                    })
                })
        })
        .await
    })
    .await
}

fn header_string(value: Option<&reqwest::header::HeaderValue>) -> Option<String> {
    value?.to_str().ok().map(str::to_owned)
}

fn provenance_media_type(url: &Url, value: Option<&reqwest::header::HeaderValue>) -> Result<String, CacheError> {
    let Some(value) = value.and_then(|value| value.to_str().ok()) else {
        return Err(peryx_upstream::UpstreamError::InvalidResponse {
            reason: format!("missing provenance Content-Type from {url}"),
        }
        .into());
    };
    let media_type = value
        .split_once(';')
        .map_or(value, |(media_type, _)| media_type)
        .trim()
        .to_ascii_lowercase();
    if matches!(
        media_type.as_str(),
        "application/vnd.pypi.integrity.v1+json" | "application/json"
    ) {
        return Ok(media_type);
    }
    Err(peryx_upstream::UpstreamError::InvalidResponse {
        reason: format!("unsupported provenance Content-Type {value:?} from {url}"),
    }
    .into())
}

fn validate_provenance(bytes: &[u8]) -> Result<(), CacheError> {
    let document: UpstreamProvenance<'_> = serde_json::from_slice(bytes).map_err(|err| {
        if err.is_data() {
            CacheError::InvalidProvenance
        } else {
            CacheError::from(err)
        }
    })?;
    if document.version != 1 || document.attestation_bundles.is_empty() {
        return Err(CacheError::InvalidProvenance);
    }
    for bundle in document.attestation_bundles {
        if bundle.publisher.kind.is_empty()
            || bundle.attestations.is_empty()
            || bundle.attestations.iter().any(|attestation| attestation.version != 1)
        {
            return Err(CacheError::InvalidProvenance);
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct UpstreamProvenance<'a> {
    version: u64,
    #[serde(borrow)]
    attestation_bundles: Vec<UpstreamAttestationBundle<'a>>,
}

#[derive(serde::Deserialize)]
struct UpstreamAttestationBundle<'a> {
    #[serde(borrow)]
    publisher: UpstreamPublisher<'a>,
    #[serde(borrow)]
    attestations: Vec<UpstreamAttestationObject<'a>>,
}

#[derive(serde::Deserialize)]
struct UpstreamPublisher<'a> {
    #[serde(borrow)]
    kind: &'a str,
    #[serde(rename = "claims", borrow, default)]
    _claims: Option<std::collections::BTreeMap<&'a str, serde_json::Value>>,
}

#[derive(serde::Deserialize)]
struct UpstreamAttestationObject<'a> {
    version: u64,
    #[serde(rename = "verification_material", borrow)]
    _verification_material: UpstreamVerificationMaterial<'a>,
    #[serde(rename = "envelope", borrow)]
    _envelope: UpstreamEnvelope<'a>,
}

#[derive(serde::Deserialize)]
struct UpstreamVerificationMaterial<'a> {
    #[serde(rename = "certificate", borrow)]
    _certificate: &'a str,
    #[serde(rename = "transparency_entries", borrow)]
    _transparency_entries: Vec<std::collections::BTreeMap<&'a str, serde_json::Value>>,
}

#[derive(serde::Deserialize)]
struct UpstreamEnvelope<'a> {
    #[serde(rename = "statement", borrow)]
    _statement: &'a str,
    #[serde(rename = "signature", borrow)]
    _signature: &'a str,
}

#[cfg(test)]
#[path = "../../tests/unit/cache/provenance/tests.rs"]
mod tests;
