use std::path::PathBuf;

use crate::config::{self, Config, PartialConfig};

mod auth_tests;
mod integration_tests;
mod jobs_tests;
mod load_tests;
mod merge_tests;
mod model_tests;
mod raw_tests;
mod replication_tests;

pub(super) fn toml_config(text: &str) -> Config {
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    Config::default().apply(partial).unwrap()
}

pub(super) fn env_partial(pairs: &[(&str, &str)]) -> Result<PartialConfig, config::ConfigError> {
    let map: std::collections::HashMap<&str, &str> = pairs.iter().copied().collect();
    config::from_env_source(|var| map.get(var).map(|value| (*value).to_owned()))
}
