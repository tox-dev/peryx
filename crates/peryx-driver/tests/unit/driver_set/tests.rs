use std::sync::Arc;

use async_trait::async_trait;
use peryx_core::Ecosystem;

use super::DriverSet;
use crate::rate_limit::RouteClass;
use crate::serving::EcosystemDriver;
use crate::state::IndexDescription;

struct Driver {
    ecosystem: Ecosystem,
}

#[async_trait]
impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    fn discover_index(&self, _index: IndexDescription, _base: Option<&crate::discovery::BaseUrl>) -> serde_json::Value {
        serde_json::Value::Null
    }
}

fn description() -> IndexDescription {
    IndexDescription {
        name: "packages".to_owned(),
        route: "packages".to_owned(),
        ecosystem: "example",
        kind: "hosted",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: None,
        hosted: None,
    }
}

#[test]
fn test_driver_set_registers_and_replaces_by_ecosystem() {
    let ecosystem = Ecosystem::new("example");
    let first: Arc<dyn EcosystemDriver> = Arc::new(Driver { ecosystem });
    let replacement: Arc<dyn EcosystemDriver> = Arc::new(Driver { ecosystem });
    let set = DriverSet::default().with(first).with(replacement.clone());

    assert!(Arc::ptr_eq(set.get(ecosystem).unwrap(), &replacement));
    assert_eq!(set.present().count(), 1);
    assert!(set.get(Ecosystem::new("missing")).is_none());
    assert_eq!(replacement.classify_route("/artifact"), RouteClass::Artifact);
    assert_eq!(replacement.discover_index(description(), None), serde_json::Value::Null);
}
