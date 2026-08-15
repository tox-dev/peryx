use super::*;

#[test]
fn test_cache_command_rejects_unknown_owner_before_opening_storage() {
    let plugins = crate::tests::support::plugins();
    let directory = tempfile::tempdir().unwrap();
    let data_dir = directory.path().join("untouched");
    let mut config = Config {
        data_dir: data_dir.clone(),
        ..Config::with_plugins(&plugins)
    };
    config.indexes[0].ecosystem = peryx_core::Ecosystem::new("missing");

    let error = crate::app::cache_with_plugins(
        &config,
        &plugins,
        &crate::cli::CacheCommand::Size(crate::cli::CacheRuntimeArgs {
            runtime: crate::cli::RuntimeArgs::default(),
        }),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(
        (format!("{error:#}"), data_dir.exists()),
        (
            "activate configured ecosystems: ecosystem missing is not installed".to_owned(),
            false,
        ),
    );
}

#[rstest::rstest]
#[case::none(AvailabilityMode::None, true)]
#[case::dc(AvailabilityMode::Dc, false)]
#[case::ha(AvailabilityMode::Ha, false)]
fn test_orphan_purge_mode_contract(#[case] mode: AvailabilityMode, #[case] supported: bool) {
    assert_eq!(validate_orphan_purge_mode(mode).is_ok(), supported);
}

#[test]
fn test_orphan_purge_report_formats_service_output() {
    let report = OrphanPurgeReport {
        blobs: vec![peryx_ha_distributed::OrphanBlob {
            digest: "abc".to_owned(),
            bytes: 6,
            path: "blobs/abc".into(),
        }],
        bytes: 6,
    };
    let mut output = Vec::new();

    write_orphan_purge_report(&mut output, "removed", &report).unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        "action\ttarget\tdigest\tsize_bytes\tpath\nremoved\torphaned-blob\tabc\t6\tblobs/abc\nsummary\tremoved\torphaned-blobs\t1\t6\n"
    );
}
