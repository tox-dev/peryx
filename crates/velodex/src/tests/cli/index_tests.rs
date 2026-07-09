use std::path::PathBuf;

use super::parse;
use crate::cli::{Command, EcosystemArg, IndexCommand};

#[test]
fn test_parse_index_list_and_show() {
    let Command::Index(list) = parse(&["velodex", "index", "list", "--ecosystem", "pypi", "--data-dir", "/d"]).command
    else {
        panic!("expected index command");
    };
    let IndexCommand::List(args) = &list else {
        panic!("expected index list");
    };
    assert_eq!(args.ecosystem, Some(EcosystemArg::Pypi));
    assert_eq!(list.runtime_args().data_dir, Some(PathBuf::from("/d")));

    let Command::Index(show) = parse(&["velodex", "index", "show", "root/pypi"]).command else {
        panic!("expected index command");
    };
    let IndexCommand::Show(args) = &show else {
        panic!("expected index show");
    };
    assert_eq!(args.index, "root/pypi");
    assert_eq!(show.runtime_args().data_dir, None);
}
