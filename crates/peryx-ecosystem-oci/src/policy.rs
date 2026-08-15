use peryx_policy::{PolicyCapabilities, PolicyLimits};
use serde::Deserialize;

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OciPolicyConfig {
    pub max_tags_per_repository: Option<u64>,
}

impl OciPolicyConfig {
    pub const KEYS: &'static [&'static str] = &["max_tags_per_repository"];
}

pub fn compile_capabilities(config: &OciPolicyConfig) -> PolicyCapabilities {
    PolicyCapabilities::default().with_limits(PolicyLimits {
        max_groups_per_resource: config.max_tags_per_repository,
        ..PolicyLimits::default()
    })
}
