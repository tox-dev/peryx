use std::path::PathBuf;

use super::parse;
use crate::cli::{Command, ConfigCheckArgs, ConfigCommand, RuntimeArgs};

#[test]
fn test_parse_config_check_carries_runtime_args() {
    let command = ConfigCommand::Check(ConfigCheckArgs {
        runtime: RuntimeArgs {
            config: Some(PathBuf::from("/etc/peryx.toml")),
            ..RuntimeArgs::default()
        },
    });
    assert_eq!(
        parse(&["peryx", "config", "check", "--config", "/etc/peryx.toml"]).command,
        Command::Config(command.clone())
    );
    assert_eq!(command.runtime_args().config, Some(PathBuf::from("/etc/peryx.toml")));
}
