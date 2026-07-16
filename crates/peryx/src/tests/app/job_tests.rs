use peryx_storage::meta::{JobKind, JobOutcome, JobState, NewJobRun};

use super::*;
use crate::app;
use crate::cli::{JobCommand, JobListArgs, JobShowArgs};

fn list_command() -> JobCommand {
    JobCommand::List(JobListArgs {
        runtime: RuntimeArgs::default(),
    })
}

fn show_command(id: &str) -> JobCommand {
    JobCommand::Show(JobShowArgs {
        runtime: RuntimeArgs::default(),
        id: id.to_owned(),
    })
}

fn start_job(meta: &MetaStore, scope: &str, started_at_unix: i64) -> String {
    meta.start_job_run(NewJobRun {
        kind: JobKind::CacheRefresh,
        scope,
        started_at_unix,
    })
    .unwrap()
}

#[test]
fn test_job_list_prints_newest_first_with_every_state() {
    let (_dir, meta, config) = store_and_config();
    let running = start_job(&meta, "", 10);
    let succeeded = start_job(&meta, "root/pypi", 20);
    meta.finish_job_run(
        &succeeded,
        JobOutcome {
            state: JobState::Succeeded,
            finished_at_unix: 21,
            items_processed: 12,
            items_changed: 3,
            error: None,
        },
    )
    .unwrap();
    let failed = start_job(&meta, "pypi", 30);
    meta.finish_job_run(
        &failed,
        JobOutcome {
            state: JobState::Failed,
            finished_at_unix: 31,
            items_processed: 4,
            items_changed: 1,
            error: Some("upstream unavailable"),
        },
    )
    .unwrap();
    drop(meta);

    let mut out = Vec::new();
    app::job(&config, &list_command(), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!(
            "id\tkind\tscope\tstate\tstarted_at_unix\tfinished_at_unix\tprocessed\tchanged\terror\n\
             {failed}\tcache_refresh\tpypi\tfailed\t30\t31\t4\t1\tupstream unavailable\n\
             {succeeded}\tcache_refresh\troot/pypi\tsucceeded\t20\t21\t12\t3\t-\n\
             {running}\tcache_refresh\t-\trunning\t10\t-\t0\t0\t-\n"
        )
    );
}

#[test]
fn test_job_list_empty_prints_header() {
    let (_dir, meta, config) = store_and_config();
    drop(meta);
    let mut out = Vec::new();
    app::job(&config, &list_command(), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "id\tkind\tscope\tstate\tstarted_at_unix\tfinished_at_unix\tprocessed\tchanged\terror\n"
    );
}

#[test]
fn test_job_show_prints_detail() {
    let (_dir, meta, config) = store_and_config();
    let id = start_job(&meta, "root/pypi", 40);
    meta.finish_job_run(
        &id,
        JobOutcome {
            state: JobState::Failed,
            finished_at_unix: 42,
            items_processed: 8,
            items_changed: 2,
            error: Some("timed out"),
        },
    )
    .unwrap();
    drop(meta);

    let mut out = Vec::new();
    app::job(&config, &show_command(&id), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!(
            "id\t{id}\nkind\tcache_refresh\nscope\troot/pypi\nstate\tfailed\nstarted_at_unix\t40\n\
             finished_at_unix\t42\nprocessed\t8\nchanged\t2\nerror\ttimed out\n"
        )
    );
}

#[test]
fn test_job_show_rejects_unknown_id() {
    let (_dir, meta, config) = store_and_config();
    drop(meta);
    let err = app::job(&config, &show_command("missing"), &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("unknown job run \"missing\""));
}

#[test]
fn test_job_reports_missing_store() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let err = app::job(&config, &list_command(), &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("open metadata store"));
}

#[test]
fn test_job_list_propagates_header_write_failure() {
    let (_dir, meta, config) = store_and_config();
    drop(meta);
    let err = app::job(&config, &list_command(), &mut FailImmediately).unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_job_list_propagates_row_write_failure() {
    let (_dir, meta, config) = store_and_config();
    start_job(&meta, "root/pypi", 50);
    drop(meta);
    let err = app::job(
        &config,
        &list_command(),
        &mut FailOnText {
            needle: "cache_refresh",
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_job_show_propagates_write_failure() {
    let (_dir, meta, config) = store_and_config();
    let id = start_job(&meta, "root/pypi", 60);
    drop(meta);
    let err = app::job(
        &config,
        &show_command(&id),
        &mut FailOnText {
            needle: "state",
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("write failed"));
}
