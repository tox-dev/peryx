use std::sync::Arc;

use async_trait::async_trait;
use peryx_ha::{
    AvailabilityAudience, AvailabilityAuthorizer, ControlActor, ControlAuthenticationError, ControlAuthorizer,
    ControlPermission, FrontierReply, MetadataFrontierProvider,
};
use peryx_identity::{Resource, Scope, UserId, parse_basic};

use crate::ServingState;

pub struct ServingStateAvailabilityAuthorizer(Arc<ServingState>);

impl ServingStateAvailabilityAuthorizer {
    #[must_use]
    pub const fn new(state: Arc<ServingState>) -> Self {
        Self(state)
    }
}

#[async_trait]
impl AvailabilityAuthorizer for ServingStateAvailabilityAuthorizer {
    async fn authorize(&self, authorization: Option<&str>) -> AvailabilityAudience {
        let Some(credentials) = authorization.and_then(parse_basic) else {
            return AvailabilityAudience::Public;
        };
        let Ok(Some(actor)) = self
            .0
            .users
            .authenticate(&credentials.user, &credentials.password)
            .await
        else {
            return AvailabilityAudience::Public;
        };
        if self
            .0
            .authorization
            .authorize_scoped(&actor, Scope::AdministrationRead, &Resource::Operator)
            .decision()
            .is_allowed()
        {
            AvailabilityAudience::Administrator
        } else if self
            .0
            .authorization
            .authorize_scoped(&actor, Scope::OperatorRead, &Resource::Operator)
            .decision()
            .is_allowed()
        {
            AvailabilityAudience::Operator
        } else {
            AvailabilityAudience::Public
        }
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
    async fn frontier(&self, authority: &str) -> Option<FrontierReply> {
        let applied_frontier = match self.0.meta.current_serial() {
            Ok(serial) => serial,
            Err(error) => {
                tracing::error!(%error, "read metadata frontier");
                return None;
            }
        };
        Some(FrontierReply {
            epoch: self.0.committed_authority_epoch(authority).await,
            applied_frontier,
        })
    }
}
