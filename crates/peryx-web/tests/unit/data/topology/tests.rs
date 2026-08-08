use super::parse_topology_snapshot;

#[test]
fn test_parse_topology_snapshot_reads_a_streamed_event() {
    let snapshot = parse_topology_snapshot(
        r#"{"mode":"dc","group":"east","captured_at":7,"node_count":1,"local":{"role":"writer","liveness":"live","frontier":42},"nodes":[]}"#,
    )
    .unwrap();
    assert_eq!(snapshot.captured_at, 7);
    assert_eq!(snapshot.local.frontier, Some(42));
}

#[test]
fn test_parse_topology_snapshot_rejects_invalid_data() {
    assert!(parse_topology_snapshot("not a snapshot").is_err());
}
