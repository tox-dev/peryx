use std::path::PathBuf;

use super::parse;
use crate::cli::{Command, IndexCommand, IndexListArgs, IndexShowArgs, RuntimeArgs};

#[test]
fn test_parse_index_list() {
    let expected = IndexCommand::List(IndexListArgs {
        runtime: RuntimeArgs {
            data_dir: Some(PathBuf::from("/d")),
            ..RuntimeArgs::default()
        },
        ecosystem: Some("artifacts".to_owned()),
    });
    let command = parse(&["peryx", "index", "list", "--ecosystem", "artifacts", "--data-dir", "/d"]).command;

    assert_eq!(command, Command::Index(expected.clone()));
    assert_eq!(expected.runtime_args().data_dir, Some(PathBuf::from("/d")));
}

#[test]
fn test_parse_index_show() {
    let expected = IndexCommand::Show(IndexShowArgs {
        runtime: RuntimeArgs::default(),
        index: "root/artifacts".to_owned(),
    });
    let command = parse(&["peryx", "index", "show", "root/artifacts"]).command;

    assert_eq!(command, Command::Index(expected.clone()));
    assert_eq!(expected.runtime_args().data_dir, None);
}
