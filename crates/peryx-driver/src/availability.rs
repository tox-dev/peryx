use std::sync::Arc;

use async_trait::async_trait;
use peryx_core::{
    BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView, OperationRow, OperationsHealth, OperationsView,
    PlacementHealth, PlacementRow, PlacementView, UiArtifactSource, UiByteAvailability, UiOperationStatus,
};
use peryx_ha::{
    ArtifactPlacementQuery, ArtifactPlacementRow, ArtifactSource, AvailabilityAudience,
    AvailabilityAuthenticationError, AvailabilityAuthorizer, AvailabilityPageQuery, AvailabilityViewReader,
    BlobPlacementRecord, BlobPlacementState, BlobPlacementViewError, ByteAvailability, ControlActor,
    ControlAuthenticationError, ControlAuthorizer, ControlPermission, FrontierReadError, FrontierReply,
    MetadataFrontierProvider, OperationsViewError, PlacementViewError,
};
use peryx_identity::{Resource, Scope, UserId, parse_basic};
use peryx_storage::meta::{
    ArtifactPlacementQueryError, OperationOutcomeQuery, OperationOutcomeQueryError, OperationOutcomeRow, OperationState,
};

use crate::ServingState;
use crate::users::{AuthenticationError, PasswordDerivationError};

pub struct ServingStateAvailabilityAuthorizer(Arc<ServingState>);

impl ServingStateAvailabilityAuthorizer {
    #[must_use]
    pub const fn new(state: Arc<ServingState>) -> Self {
        Self(state)
    }

    async fn authorize_local(
        &self,
        authorization: Option<&str>,
    ) -> Result<AvailabilityAudience, PasswordDerivationError> {
        let Some(credentials) = authorization.and_then(parse_basic) else {
            return Ok(AvailabilityAudience::Public);
        };
        let actor = match self
            .0
            .users
            .authenticate(&credentials.user, &credentials.password)
            .await
        {
            Ok(Some(actor)) => actor,
            Ok(None) | Err(AuthenticationError::Store(_)) => return Ok(AvailabilityAudience::Public),
            Err(AuthenticationError::Derivation(error)) => return Err(error),
        };
        if self
            .0
            .authorization
            .authorize_scoped(&actor, Scope::AdministrationRead, &Resource::Operator)
            .decision()
            .is_allowed()
        {
            Ok(AvailabilityAudience::Administrator)
        } else if self
            .0
            .authorization
            .authorize_scoped(&actor, Scope::OperatorRead, &Resource::Operator)
            .decision()
            .is_allowed()
        {
            Ok(AvailabilityAudience::Operator)
        } else {
            Ok(AvailabilityAudience::Public)
        }
    }
}

#[async_trait]
impl AvailabilityAuthorizer for ServingStateAvailabilityAuthorizer {
    async fn authorize(
        &self,
        authorization: Option<&str>,
    ) -> Result<AvailabilityAudience, AvailabilityAuthenticationError> {
        self.authorize_local(authorization)
            .await
            .map_err(|_| AvailabilityAuthenticationError)
    }
}

pub struct ServingStateControlAuthorizer(Arc<ServingState>);

impl ServingStateControlAuthorizer {
    #[must_use]
    pub const fn new(state: Arc<ServingState>) -> Self {
        Self(state)
    }
}

#[async_trait]
impl ControlAuthorizer for ServingStateControlAuthorizer {
    async fn authenticate(
        &self,
        authorization: Option<&str>,
    ) -> Result<Option<ControlActor>, ControlAuthenticationError> {
        let Some(credentials) = authorization.and_then(parse_basic) else {
            return Ok(None);
        };
        let actor = self
            .0
            .users
            .authenticate(&credentials.user, &credentials.password)
            .await
            .or(Err(ControlAuthenticationError))?;
        Ok(actor.map(|actor| ControlActor::new(actor.to_string())))
    }

    fn allows(&self, actor: &ControlActor, permission: ControlPermission) -> bool {
        let scope = match permission {
            ControlPermission::Read => Scope::AdministrationRead,
            ControlPermission::Write => Scope::AdministrationWrite,
        };
        self.0
            .authorization
            .authorize_scoped(&UserId::from_stored(actor.as_str()), scope, &Resource::Operator)
            .decision()
            .is_allowed()
    }
}

pub struct ServingStateMetadataFrontierProvider(Arc<ServingState>);

impl ServingStateMetadataFrontierProvider {
    #[must_use]
    pub const fn new(state: Arc<ServingState>) -> Self {
        Self(state)
    }
}

#[async_trait]
impl MetadataFrontierProvider for ServingStateMetadataFrontierProvider {
    async fn frontier(&self, authority: &str) -> Result<Option<FrontierReply>, FrontierReadError> {
        let applied_frontier = match self.0.meta.current_serial() {
            Ok(serial) => serial,
            Err(error) => {
                tracing::error!(%error, "read metadata frontier");
                return Err(FrontierReadError);
            }
        };
        Ok(Some(FrontierReply {
            epoch: self.0.committed_authority_epoch(authority).await,
            applied_frontier,
        }))
    }
}

impl AvailabilityViewReader for ServingState {
    fn placement_view(&self, query: AvailabilityPageQuery) -> Result<PlacementView, PlacementViewError> {
        let (rows, next_cursor) = if query.include_rows {
            match self.meta.list_artifact_placements(&ArtifactPlacementQuery {
                cursor: query.cursor,
                limit: query.limit,
            }) {
                Ok(page) => (
                    Some(page.rows.into_iter().map(placement_row).collect()),
                    page.next_cursor,
                ),
                Err(ArtifactPlacementQueryError::InvalidLimit) => return Err(PlacementViewError::InvalidLimit),
                Err(ArtifactPlacementQueryError::Store(_)) => return Err(PlacementViewError::RowsRead),
            }
        } else {
            (None, None)
        };
        let health = self
            .meta
            .artifact_placement_health()
            .map_err(|_| PlacementViewError::HealthRead)?;
        Ok(PlacementView {
            captured_at: (self.clock)(),
            health: PlacementHealth {
                local: health.local,
                remote_only: health.remote_only,
                unavailable: health.unavailable,
                total: health.total(),
            },
            rows,
            next_cursor,
        })
    }

    fn blob_placement_view(&self, digest: &str) -> Result<BlobPlacementView, BlobPlacementViewError> {
        let digest = digest.parse().map_err(|_| BlobPlacementViewError::InvalidDigest)?;
        let records = self
            .meta
            .blob_placements(&digest)
            .map_err(|_| BlobPlacementViewError::Read)?;
        let mut datacenters: Vec<BlobDatacenterPlacement> = records.iter().map(datacenter_placement).collect();
        datacenters.sort_by(|left, right| {
            left.data_center
                .cmp(&right.data_center)
                .then(left.updated_at.cmp(&right.updated_at))
        });
        Ok(BlobPlacementView {
            digest: digest.canonical(),
            datacenters,
        })
    }

    fn operations_view(&self, query: AvailabilityPageQuery) -> Result<OperationsView, OperationsViewError> {
        let now = (self.clock)();
        let (rows, next_cursor) = if query.include_rows {
            match self.meta.list_operation_outcomes(&OperationOutcomeQuery {
                cursor: query.cursor,
                limit: query.limit,
            }) {
                Ok(page) => (
                    Some(page.rows.into_iter().map(|row| operation_row(row, now)).collect()),
                    page.next_cursor,
                ),
                Err(OperationOutcomeQueryError::InvalidLimit) => return Err(OperationsViewError::InvalidLimit),
                Err(OperationOutcomeQueryError::Store(_)) => return Err(OperationsViewError::RowsRead),
            }
        } else {
            (None, None)
        };
        let health = self
            .meta
            .operation_outcome_health(now)
            .map_err(|_| OperationsViewError::HealthRead)?;
        Ok(OperationsView {
            captured_at: now,
            health: OperationsHealth {
                pending: health.pending,
                published: health.published,
                failed: health.failed,
                expired: health.expired,
                total: health.total(),
            },
            rows,
            next_cursor,
        })
    }
}

fn placement_row(row: ArtifactPlacementRow) -> PlacementRow {
    PlacementRow {
        digest: row.digest,
        source: match row.source {
            ArtifactSource::Hosted => UiArtifactSource::Hosted,
            ArtifactSource::Proxy => UiArtifactSource::Proxy,
            ArtifactSource::Generated => UiArtifactSource::Generated,
        },
        availability: match row.availability {
            ByteAvailability::Local => UiByteAvailability::Local,
            ByteAvailability::RemoteOnly => UiByteAvailability::RemoteOnly,
            ByteAvailability::Unavailable => UiByteAvailability::Unavailable,
        },
    }
}

fn datacenter_placement(record: &BlobPlacementRecord) -> BlobDatacenterPlacement {
    let (status, size) = match record.state {
        BlobPlacementState::Pending => (BlobPlacementStatus::Pending, None),
        BlobPlacementState::Verified { size } => (BlobPlacementStatus::Verified, Some(size)),
        BlobPlacementState::Failed { .. } => (BlobPlacementStatus::Failed, None),
        BlobPlacementState::Revoked => (BlobPlacementStatus::Revoked, None),
    };
    BlobDatacenterPlacement {
        data_center: record.key.data_center.as_str().to_owned(),
        status,
        size,
        updated_at: record.updated_at_unix,
    }
}

fn operation_row(row: OperationOutcomeRow, now: i64) -> OperationRow {
    OperationRow {
        operation: row.operation,
        status: UiOperationStatus::derive(
            matches!(row.state, OperationState::Published),
            matches!(row.state, OperationState::Failed),
            row.expiry_unix,
            now,
        ),
        updated_at: row.updated_at_unix,
        expires_at: row.expiry_unix,
    }
}

#[cfg(test)]
#[path = "../tests/unit/availability/tests.rs"]
mod tests;
