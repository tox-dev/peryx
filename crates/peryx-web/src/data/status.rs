use crate::model::{UiSnapshot, UiStats};

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use super::RequiredOption;
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use crate::model::{UiEcosystemSummary, UiHosted, UiIndex, UiMetricFamily, UiRecentWrite, UiSummaryStatus, UiUpstream};

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct StatusDocument {
    version: String,
    #[serde(default)]
    serial: Option<u64>,
    #[serde(default)]
    requests: u64,
    #[serde(default, rename = "by_ecosystem")]
    ecosystems: Vec<UiEcosystemSummary>,
    #[serde(default, rename = "metric_families")]
    families: Vec<UiMetricFamily>,
    indexes: Vec<StatusIndex>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct StatusIndex {
    name: String,
    route: String,
    ecosystem: String,
    endpoint: String,
    kind: String,
    layers: Vec<String>,
    uploads: bool,
    upload_to: RequiredOption<String>,
    upstream: Option<StatusUpstream>,
    hosted: Option<StatusHosted>,
    summary: Option<StatusSummary>,
    resource_count: Option<u64>,
    write_count: Option<u64>,
    recent_writes: Option<Vec<StatusRecentWrite>>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct StatusUpstream {
    url: String,
    auth: StatusAuth,
    status: String,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct StatusAuth {
    kind: String,
    redacted: RequiredOption<String>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct StatusHosted {
    volatile: bool,
    upload_token: StatusUploadToken,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct StatusUploadToken {
    configured: bool,
    redacted: RequiredOption<String>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
enum StatusSummary {
    Available,
    Unavailable { error_class: String },
    Unsupported,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct StatusRecentWrite {
    resource: String,
    artifact: String,
    group: String,
    written_at: RequiredOption<String>,
    size: RequiredOption<u64>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
impl TryFrom<StatusDocument> for UiSnapshot {
    type Error = super::LoaderError;

    fn try_from(document: StatusDocument) -> Result<Self, Self::Error> {
        Ok(Self {
            version: document.version,
            serial: document.serial,
            requests: document.requests,
            ecosystems: document.ecosystems,
            families: document.families,
            indexes: document
                .indexes
                .into_iter()
                .map(UiIndex::try_from)
                .collect::<Result<_, _>>()?,
        })
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
impl TryFrom<StatusIndex> for UiIndex {
    type Error = super::LoaderError;

    fn try_from(index: StatusIndex) -> Result<Self, Self::Error> {
        let (summary_status, summary_error_class, resource_count, write_count, recent_writes) = match index.summary {
            Some(StatusSummary::Available) => (
                UiSummaryStatus::Available,
                None,
                required(index.resource_count)?,
                required(index.write_count)?,
                required(index.recent_writes)?
                    .into_iter()
                    .map(|write| UiRecentWrite {
                        resource: write.resource,
                        artifact: write.artifact,
                        group: write.group,
                        written_at: write.written_at.0,
                        size: write.size.0,
                    })
                    .collect(),
            ),
            Some(StatusSummary::Unavailable { error_class }) => {
                (UiSummaryStatus::Unavailable, Some(error_class), 0, 0, Vec::new())
            }
            Some(StatusSummary::Unsupported) | None => (UiSummaryStatus::Unsupported, None, 0, 0, Vec::new()),
        };
        Ok(Self {
            name: index.name,
            route: index.route,
            ecosystem: index.ecosystem,
            endpoint: index.endpoint,
            kind: index.kind,
            layers: index.layers,
            uploads: index.uploads,
            upload_to: index.upload_to.0,
            upstream: index.upstream.map(|upstream| UiUpstream {
                url: upstream.url,
                auth_kind: upstream.auth.kind,
                auth_redacted: upstream.auth.redacted.0,
                status: upstream.status,
            }),
            hosted: index.hosted.map(|hosted| UiHosted {
                volatile: hosted.volatile,
                token_configured: hosted.upload_token.configured,
                token_redacted: hosted.upload_token.redacted.0,
            }),
            summary_status,
            summary_error_class,
            resource_count,
            write_count,
            recent_writes,
        })
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn required<T>(value: Option<T>) -> Result<T, super::LoaderError> {
    value.ok_or(super::LoaderError::Invalid(super::LoaderEndpoint::Status))
}

/// A status snapshot and the usage counters read in the same refresh.
///
/// The usage half carries its own outcome because `/+stats` is operator-scoped: a reader without
/// that scope is answered `401` on every poll, and failing the pair on it would leave the whole
/// dashboard blank rather than show the indexes it is allowed to see.
pub type UiOverview = (UiSnapshot, Result<UiStats, super::LoaderError>);

/// The dashboard snapshot and the usage counters measured beside it.
///
/// Status and usage are separate endpoints that both keep counting while a page is open, so they
/// are read as one pair: an index card must never report listings counted seconds apart from the
/// accepted-request total printed above it, a combination the server never held. A pair whose
/// usage half failed reports that failure instead of the counters, never counters from an earlier
/// refresh.
///
/// # Errors
///
/// Returns a typed error when the status endpoint cannot provide a valid document. Nothing is
/// published then, so the caller keeps the last pair it did publish.
pub async fn load_overview() -> Result<UiOverview, super::LoaderError> {
    let (snapshot, usage) = futures_util::future::join(load_snapshot(), super::load_stats(None, None)).await;
    Ok((snapshot?, usage))
}

/// The admin snapshot and the usage counters measured beside it, paired as [`load_overview`] pairs
/// them.
///
/// # Errors
///
/// Returns a typed error when the status endpoint cannot provide a valid document.
pub async fn load_admin_overview() -> Result<UiOverview, super::LoaderError> {
    let (snapshot, usage) = futures_util::future::join(load_admin_snapshot(), super::load_stats(None, None)).await;
    Ok((snapshot?, usage))
}

/// The dashboard snapshot.
///
/// # Errors
///
/// Returns a typed error when the status endpoint cannot provide a valid document.
async fn load_snapshot() -> Result<UiSnapshot, super::LoaderError> {
    #[cfg(feature = "ssr")]
    {
        Ok(crate::ssr::snapshot().await)
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async {
            let document: StatusDocument =
                super::fetch_json_required("/+status", super::LoaderEndpoint::Status).await?;
            document.try_into()
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(UiSnapshot::default())
    }
}

/// The admin status snapshot, including bounded metadata summaries.
///
/// # Errors
///
/// Returns a typed error when the status endpoint cannot provide a valid document.
async fn load_admin_snapshot() -> Result<UiSnapshot, super::LoaderError> {
    #[cfg(feature = "ssr")]
    {
        Ok(crate::ssr::admin_snapshot().await)
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async {
            let document: StatusDocument =
                super::fetch_json_required("/+status", super::LoaderEndpoint::Status).await?;
            document.try_into()
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(UiSnapshot::default())
    }
}
