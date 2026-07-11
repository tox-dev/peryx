#[test]
fn test_open_rebuilds_when_the_on_disk_schema_changed() {
    let dir = tempfile::tempdir().unwrap();
    // Leave an index a prior peryx built with a different schema.
    let mut legacy = tantivy::schema::Schema::builder();
    legacy.add_text_field("legacy", tantivy::schema::TEXT);
    tantivy::Index::builder()
        .schema(legacy.build())
        .create_in_dir(dir.path())
        .expect("create the legacy index");
    // Opening discards the mismatched index and rebuilds in place instead of failing startup.
    crate::PackageSearch::open(dir.path()).expect("open rebuilds a mismatched index");
}
