#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use crate::model::{AnalyticsView, UiUsagePage};

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct CounterGroups {
    base: BaseCounters,
    cached: CachedCounters,
    hosted: HostedCounters,
    ecosystem: std::collections::BTreeMap<String, u64>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct BaseCounters {
    pages: u64,
    reads: u64,
    bytes: u64,
    rejected: u64,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct CachedCounters {
    refreshes: u64,
    changed: u64,
    stale_served: u64,
    upstream_errors: u64,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct HostedCounters {
    writes: u64,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct IndexStatsDocument {
    #[serde(default)]
    totals: PresentField<CounterGroups>,
    #[serde(default)]
    resources: PresentField<std::collections::BTreeMap<String, CounterGroups>>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct ResourceStatsDocument {
    #[serde(default)]
    totals: PresentField<CounterGroups>,
    #[serde(default)]
    artifacts: PresentField<std::collections::BTreeMap<String, ArtifactCounters>>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(Default)]
enum PresentField<T> {
    #[default]
    Missing,
    Value(T),
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for PresentField<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Value)
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct ArtifactCounters {
    reads: u64,
    bytes: u64,
    ecosystem: std::collections::BTreeMap<String, u64>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
impl From<CounterGroups> for crate::model::UiCounters {
    fn from(counters: CounterGroups) -> Self {
        Self {
            pages: counters.base.pages,
            reads: counters.base.reads,
            metadata: counters.ecosystem.get("metadata").copied().unwrap_or(0),
            writes: counters.hosted.writes,
            bytes: counters.base.bytes,
            refreshes: counters.cached.refreshes,
            changed: counters.cached.changed,
            stale_served: counters.cached.stale_served,
            upstream_errors: counters.cached.upstream_errors,
            rejected: counters.base.rejected,
        }
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
impl From<ArtifactCounters> for crate::model::UiCounters {
    fn from(counters: ArtifactCounters) -> Self {
        Self {
            reads: counters.reads,
            bytes: counters.bytes,
            metadata: counters.ecosystem.get("metadata").copied().unwrap_or(0),
            ..Self::default()
        }
    }
}

/// Load usage counters at the selected drill depth.
///
/// # Errors
///
/// Returns a typed error when the stats endpoint cannot provide a valid document.
pub async fn load_stats(
    index: Option<String>,
    resource: Option<String>,
) -> Result<crate::model::UiStats, super::LoaderError> {
    #[cfg(feature = "ssr")]
    {
        Ok(parse_stats(
            &crate::ssr::stats(index.as_deref(), resource.as_deref()).await,
            index.as_deref(),
            resource.as_deref(),
        ))
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async move {
            let url = crate::url::stats_api_url(index.as_deref(), resource.as_deref());
            match (index.as_deref(), resource.as_deref()) {
                (Some(_), Some(_)) => {
                    let document: ResourceStatsDocument =
                        super::fetch_json_required(&url, super::LoaderEndpoint::Stats).await?;
                    stats_resource(document)
                }
                (Some(_), None) => {
                    let document: IndexStatsDocument =
                        super::fetch_json_required(&url, super::LoaderEndpoint::Stats).await?;
                    stats_index(document)
                }
                (None, _) => {
                    let routes: std::collections::BTreeMap<String, CounterGroups> =
                        super::fetch_json_required(&url, super::LoaderEndpoint::Stats).await?;
                    Ok(stats_routes(routes))
                }
            }
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        let _ = (index, resource);
        Ok(crate::model::UiStats::default())
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn stats_routes(routes: std::collections::BTreeMap<String, CounterGroups>) -> crate::model::UiStats {
    let mut rows = routes
        .into_iter()
        .map(|(name, counters)| (name, counters.into()))
        .collect::<Vec<_>>();
    sort_rows(&mut rows);
    crate::model::UiStats {
        totals: rows
            .iter()
            .fold(crate::model::UiCounters::default(), |mut total, (_, counters)| {
                total.pages += counters.pages;
                total.reads += counters.reads;
                total.metadata += counters.metadata;
                total.writes += counters.writes;
                total.bytes += counters.bytes;
                total.refreshes += counters.refreshes;
                total.changed += counters.changed;
                total.stale_served += counters.stale_served;
                total.upstream_errors += counters.upstream_errors;
                total.rejected += counters.rejected;
                total
            }),
        rows,
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn stats_index(document: IndexStatsDocument) -> Result<crate::model::UiStats, super::LoaderError> {
    stats_page(document.totals, document.resources)
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn stats_resource(document: ResourceStatsDocument) -> Result<crate::model::UiStats, super::LoaderError> {
    stats_page(document.totals, document.artifacts)
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn stats_page<T>(
    totals: PresentField<CounterGroups>,
    rows: PresentField<std::collections::BTreeMap<String, T>>,
) -> Result<crate::model::UiStats, super::LoaderError>
where
    T: Into<crate::model::UiCounters>,
{
    match (totals, rows) {
        (PresentField::Missing, PresentField::Missing) => Ok(crate::model::UiStats::default()),
        (PresentField::Value(totals), PresentField::Value(rows)) => {
            let mut rows = rows
                .into_iter()
                .map(|(name, counters)| (name, counters.into()))
                .collect::<Vec<_>>();
            sort_rows(&mut rows);
            Ok(crate::model::UiStats {
                totals: totals.into(),
                rows,
            })
        }
        _ => Err(super::LoaderError::Invalid(super::LoaderEndpoint::Stats)),
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn sort_rows(rows: &mut [(String, crate::model::UiCounters)]) {
    rows.sort_by(|(left_name, left), (right_name, right)| {
        (right.reads + right.pages)
            .cmp(&(left.reads + left.pages))
            .then_with(|| left_name.cmp(right_name))
    });
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
/// # Errors
///
/// Returns an error when the service rejects the request or returns invalid data.
pub async fn load_analytics(url: &str, view: AnalyticsView, user: &str, password: &str) -> Result<UiUsagePage, String> {
    send_wrapper::SendWrapper::new(async move {
        use base64::Engine as _;

        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        let response = gloo_net::http::Request::get(url)
            .header("accept", "application/json")
            .header("authorization", &format!("Basic {credentials}"))
            .send()
            .await
            .map_err(|_| "The analytics service could not be reached.".to_owned())?;
        match response.status() {
            200 => {
                let value = response
                    .json()
                    .await
                    .map_err(|_| "The analytics service returned invalid data.".to_owned())?;
                UiUsagePage::parse(view, &value)
                    .ok_or_else(|| "The analytics service returned invalid data.".to_owned())
            }
            400 => Err("One or more analytics filters are invalid.".to_owned()),
            401 => Err("The username or password was not accepted.".to_owned()),
            403 => Err("This repository token cannot inspect usage analytics.".to_owned()),
            404 => Err("The repository was not found or is not available to this user.".to_owned()),
            _ => Err("The analytics service is unavailable.".to_owned()),
        }
    })
    .await
}

#[cfg(feature = "ssr")]
fn parse_stats(value: &serde_json::Value, index: Option<&str>, resource: Option<&str>) -> crate::model::UiStats {
    match (index, resource) {
        (Some(_), Some(_)) => crate::model::stats_resource(value),
        (Some(_), None) => crate::model::stats_index(value),
        (None, _) => crate::model::stats_routes(value),
    }
}

#[cfg(test)]
#[cfg(feature = "ssr")]
#[path = "../../tests/unit/data/stats/tests.rs"]
mod tests;
