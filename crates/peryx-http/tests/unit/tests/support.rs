use peryx_core::Ecosystem;
use peryx_driver::serving::{
    AbsoluteProtocolDriver, CapabilityRegistrar, ClientDiscovery, EcosystemDriver, IndexCredentialDriver, IndexSummary,
    IndexSummaryDriver, IndexSummaryError, RecentWrite,
};
use peryx_driver::state::{AppState, Index, IndexDescription};
use peryx_identity::{Action, Denial, authorize_all, parse_basic};
use peryx_test_support::EcosystemDriverFixture;

pub const EXTERNAL_USER: &str = "external";
pub const EXAMPLE_CREDENTIALS: ExampleCredentials = ExampleCredentials;
static EXAMPLE_DRIVER: EcosystemDriverFixture = EcosystemDriverFixture::new(
    Ecosystem::new("example"),
    peryx_driver::rate_limit::RouteClass::Artifact,
)
.with_capabilities(example_capabilities);

pub struct ExampleCredentials;

impl IndexCredentialDriver for ExampleCredentials {
    fn recognizes(&self, authorization: &str) -> bool {
        parse_basic(authorization).is_some_and(|credentials| credentials.user == EXTERNAL_USER)
    }

    fn authorize(&self, index: &Index, authorization: Option<&str>, action: Action, now: i64) -> Result<(), Denial> {
        if !authorization.is_some_and(|value| self.recognizes(value)) {
            return Err(Denial::Unauthenticated);
        }
        authorize_all(&index.acl.identify(authorization, now).principal, &index.acl, action)
    }
}

struct ExampleSummary;

impl IndexSummaryDriver for ExampleSummary {
    fn summarize_indexes(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
        _recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, IndexSummaryError> {
        if index_names.iter().any(|name| name == "summary-failure") {
            return Err(IndexSummaryError::Storage);
        }
        Ok(index_names
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    IndexSummary {
                        resource_count: 1,
                        write_count: 1,
                        recent_writes: vec![RecentWrite {
                            resource: "resource".to_owned(),
                            artifact: "artifact.bin".to_owned(),
                            group: "group".to_owned(),
                            written_at: Some("2026-01-01T00:00:00Z".to_owned()),
                            size: Some(8),
                        }],
                    },
                )
            })
            .collect())
    }
}

pub fn register_example_driver(state: &mut AppState) {
    state.register_capabilities(example_capabilities);
    EXAMPLE_DRIVER.register_with_discovery(state);
}

fn example_capabilities(registrar: &mut dyn CapabilityRegistrar) {
    registrar.register_index_credentials(Ecosystem::new("example"), std::sync::Arc::new(ExampleCredentials));
    registrar.register_index_summary(Ecosystem::new("example"), std::sync::Arc::new(ExampleSummary));
}

#[test]
fn test_example_driver_contract() {
    let driver = &EXAMPLE_DRIVER;
    let description = IndexDescription {
        name: "example".to_owned(),
        route: "example".to_owned(),
        ecosystem: "example".to_owned(),
        kind: "hosted",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: None,
        hosted: None,
    };

    assert_eq!(driver.ecosystem(), Ecosystem::new("example"));
    assert_eq!(
        AbsoluteProtocolDriver::classify_route(driver, "/example"),
        peryx_driver::rate_limit::RouteClass::Artifact
    );
    assert_eq!(
        driver.discover_index(description.clone(), None),
        peryx_driver::discovery::minimal_entry(&description)
    );
    assert_eq!(
        EXAMPLE_CREDENTIALS.authorize(
            &Index {
                name: "example".to_owned(),
                route: "example".to_owned(),
                ecosystem: Ecosystem::new("example"),
                kind: peryx_driver::IndexKind::Hosted { volatile: false },
                policy: peryx_policy::Policy::default(),
                acl: peryx_identity::IndexAcl::default(),
            },
            None,
            Action::Read,
            0,
        ),
        Err(Denial::Unauthenticated)
    );
}
