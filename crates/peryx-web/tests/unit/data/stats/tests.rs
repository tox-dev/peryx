#[test]
fn stats_parser_selects_requested_depth() {
    let value = serde_json::json!({
        "routes": {"route": {}},
        "resources": {"resource": {}},
        "artifacts": {"artifact": {}},
    });
    for (index, resource, expected) in [
        (None, None, "artifacts"),
        (None, Some("artifact"), "artifacts"),
        (Some("root/cache"), None, "resource"),
        (Some("root/cache"), Some("artifact"), "artifact"),
    ] {
        assert_eq!(super::parse_stats(&value, index, resource).rows[0].0, expected);
    }
}
