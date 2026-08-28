use peryx_identity::Action;
use peryx_index::{Index, IndexKind, shadow_order};
use peryx_upstream::{UpstreamHealth, UpstreamRouter};

/// Describe every runtime index without touching storage or upstream state.
#[must_use]
pub fn describe_indexes(indexes: &[Index]) -> Vec<IndexDescription> {
    let now = system_now();
    (0..indexes.len())
        .map(|position| describe_index_at(indexes, position, now))
        .collect()
}

#[must_use]
pub fn describe_index(indexes: &[Index], position: usize) -> IndexDescription {
    describe_index_at(indexes, position, system_now())
}

fn describe_index_at(indexes: &[Index], position: usize, now: i64) -> IndexDescription {
    let index = &indexes[position];
    let (layers, precedence, uploads, volatile_deletes, upload_to) = match &index.kind {
        IndexKind::Cached { .. } => (Vec::new(), Vec::new(), false, false, None),
        IndexKind::Hosted { .. } => (
            Vec::new(),
            Vec::new(),
            active(index, Action::Write, now),
            active(index, Action::Delete, now) && volatile(index),
            None,
        ),
        IndexKind::Virtual { layers, write_target } => {
            let names = layers.iter().map(|&pos| indexes[pos].name.clone()).collect();
            let precedence = shadow_order(indexes, layers)
                .into_iter()
                .map(|pos| MemberDescription {
                    name: indexes[pos].name.clone(),
                    role: kind_str(&indexes[pos].kind),
                })
                .collect();
            let target = write_target.map(|pos| &indexes[pos]);
            let uploads = target.is_some_and(|index| active(index, Action::Write, now));
            let volatile_deletes = target.is_some_and(|index| active(index, Action::Delete, now) && volatile(index));
            let upload_to = target.map(|index| index.name.clone());
            (names, precedence, uploads, volatile_deletes, upload_to)
        }
    };
    let (upstream, hosted) = match &index.kind {
        IndexKind::Cached { client, offline } => (
            Some(UpstreamDescription {
                url: client.redacted_base_url(),
                auth: client.auth_status().as_str(),
                offline: *offline,
                status: "configured",
                sources: Vec::new(),
            }),
            None,
        ),
        IndexKind::Hosted { volatile } => (
            None,
            Some(HostedDescription {
                volatile: *volatile,
                upload_token: SecretDescription::new(index.acl.grants_to_anyone(Action::Write)),
            }),
        ),
        IndexKind::Virtual { .. } => (None, None),
    };
    IndexDescription {
        name: index.name.clone(),
        route: index.route.clone(),
        ecosystem: index.ecosystem.as_str().to_owned(),
        kind: kind_str(&index.kind),
        layers,
        precedence,
        uploads,
        volatile_deletes,
        upload_to,
        upstream,
        hosted,
    }
}

/// The stable role name of an index kind, shared by the top-level `kind` and each virtual member's
/// `role`, so the two never drift.
const fn kind_str(kind: &IndexKind) -> &'static str {
    match kind {
        IndexKind::Cached { .. } => "cached",
        IndexKind::Hosted { .. } => "hosted",
        IndexKind::Virtual { .. } => "virtual",
    }
}

fn active(index: &Index, action: Action, now: i64) -> bool {
    index.acl.grants_to_anyone_at(action, now)
}

const fn volatile(index: &Index) -> bool {
    matches!(index.kind, IndexKind::Hosted { volatile: true })
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

/// A configured index as presented to humans: on the dashboard, in `/+status`, and in discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescription {
    pub name: String,
    pub route: String,
    pub ecosystem: String,
    pub kind: &'static str,
    /// A virtual index's members named in the operator's configured order; empty otherwise.
    pub layers: Vec<String>,
    /// A virtual index's members in the order requests actually merge them - cached members forced
    /// last whatever the configured `layers` order, so an earlier entry shadows a later one. Each
    /// carries its role, distinguishing a local hosted source from a proxied upstream. Empty for a
    /// non-virtual index.
    pub precedence: Vec<MemberDescription>,
    pub uploads: bool,
    pub volatile_deletes: bool,
    /// For a virtual index: the layer uploads land in, whether or not a token currently enables them.
    pub upload_to: Option<String>,
    pub upstream: Option<UpstreamDescription>,
    pub hosted: Option<HostedDescription>,
}

/// One member of a virtual index as a status surface presents it: its name and role, positioned by
/// [`IndexDescription::precedence`] so its rank shows which member shadows which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberDescription {
    pub name: String,
    pub role: &'static str,
}

/// A cached index's upstream status, with credential material excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamDescription {
    pub url: String,
    pub auth: &'static str,
    pub offline: bool,
    pub status: &'static str,
    pub sources: Vec<UpstreamSourceDescription>,
}

/// One named source in a cached index's upstream route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamSourceDescription {
    pub name: String,
    pub url: String,
    pub auth: &'static str,
    pub status: &'static str,
}

pub(super) fn describe_upstream_route(router: &UpstreamRouter) -> (&'static str, Vec<UpstreamSourceDescription>) {
    let sources = router
        .sources()
        .map(|source| UpstreamSourceDescription {
            name: source.name().to_owned(),
            url: source.client().redacted_base_url(),
            auth: source.client().auth_status().as_str(),
            status: source.health().as_str(),
        })
        .collect::<Vec<_>>();
    let healthy = sources
        .iter()
        .filter(|source| source.status == UpstreamHealth::Healthy.as_str())
        .count();
    let unhealthy = sources
        .iter()
        .filter(|source| source.status == UpstreamHealth::Unhealthy.as_str())
        .count();
    let status = match (healthy, unhealthy) {
        (0, 0) => "configured",
        (0, _) => "unhealthy",
        (_, 0) => "healthy",
        _ => "degraded",
    };
    (status, sources)
}

/// A hosted store's status, with upload-token values excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedDescription {
    pub volatile: bool,
    pub upload_token: SecretDescription,
}

/// Redacted secret metadata for status surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretDescription {
    pub configured: bool,
    pub redacted: Option<&'static str>,
}

impl SecretDescription {
    #[must_use]
    pub fn new(configured: bool) -> Self {
        Self {
            configured,
            redacted: configured.then_some("<redacted>"),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/state/describe/tests.rs"]
mod tests;
