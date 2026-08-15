use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use peryx_core::Ecosystem;
use peryx_driver::discovery::{BaseUrl, minimal_entry};
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{AbsoluteProtocolDriver, CapabilityRegistrar, ClientDiscovery, EcosystemDriver};
use peryx_driver::state::{AppState, IndexDescription, ServingState};

#[derive(Clone)]
pub struct EcosystemDriverFixture {
    ecosystem: Ecosystem,
    route_class: RouteClass,
    register: fn(&mut dyn CapabilityRegistrar),
}

impl EcosystemDriverFixture {
    #[must_use]
    pub const fn new(ecosystem: Ecosystem, route_class: RouteClass) -> Self {
        Self {
            ecosystem,
            route_class,
            register: |_| {},
        }
    }

    #[must_use]
    pub const fn with_capabilities(mut self, register: fn(&mut dyn CapabilityRegistrar)) -> Self {
        self.register = register;
        self
    }

    pub fn register(self, state: &mut AppState) {
        state.register_capabilities(self.register);
        state.register_driver(Arc::new(self));
    }

    pub fn register_with_discovery(&'static self, state: &mut AppState) {
        state.register_driver(Arc::new(self.clone()));
        state.register_client_discovery(self.ecosystem.clone(), self);
    }
}

impl EcosystemDriver for EcosystemDriverFixture {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem.clone()
    }
}

impl ClientDiscovery for EcosystemDriverFixture {
    fn discover_index(&self, index: IndexDescription, _base: Option<&BaseUrl>) -> serde_json::Value {
        minimal_entry(&index)
    }

    fn client_endpoint(&self, route: &str) -> String {
        format!("/{route}/")
    }
}

#[async_trait::async_trait]
impl AbsoluteProtocolDriver for EcosystemDriverFixture {
    fn prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        self.route_class
    }

    async fn serve(&self, _state: Arc<ServingState>, _request: Request) -> Response {
        StatusCode::NOT_FOUND.into_response()
    }
}
