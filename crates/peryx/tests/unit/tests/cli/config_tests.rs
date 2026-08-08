use std::path::PathBuf;

use super::parse;
use crate::cli::{Command, ConfigCommand};

#[test]
fn test_parse_config_check_carries_runtime_args() {
    let Command::Config(command) = parse(&["peryx", "config", "check", "--config", "/etc/peryx.toml"]).command else {
        panic!("expected config command");
    };
    let ConfigCommand::Check(args) = &command;
    assert_eq!(args.runtime.config, Some(PathBuf::from("/etc/peryx.toml")));
    assert_eq!(command.runtime_args().config, Some(PathBuf::from("/etc/peryx.toml")));
}
