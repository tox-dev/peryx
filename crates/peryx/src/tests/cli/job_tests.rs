use std::path::PathBuf;

use super::parse;
use crate::cli::{Command, JobCommand};

#[test]
fn test_parse_job_list_and_show() {
    let Command::Job(list) = parse(&["peryx", "job", "list", "--data-dir", "/d"]).command else {
        panic!("expected job command");
    };
    assert!(matches!(list, JobCommand::List(_)));
    assert_eq!(list.runtime_args().data_dir, Some(PathBuf::from("/d")));

    let Command::Job(show) = parse(&["peryx", "job", "show", "jr_0000000000000001"]).command else {
        panic!("expected job command");
    };
    let JobCommand::Show(args) = &show else {
        panic!("expected job show");
    };
    assert_eq!(args.id, "jr_0000000000000001");
    assert_eq!(show.runtime_args().data_dir, None);
}
