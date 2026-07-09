use velodex_format::{Ecosystem, Lexicon};

use super::{OCI_WORDS, state};

#[test]
fn test_lexicon_defaults_to_neutral_then_uses_the_registered_words() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir);
    // Before registration, every ecosystem reads velodex's neutral vocabulary.
    assert_eq!(state.lexicon(Ecosystem::Oci), &Lexicon::NEUTRAL);
    state.register_lexicon(Ecosystem::Oci, &OCI_WORDS);
    assert_eq!(state.lexicon(Ecosystem::Oci).collection, "repository");
    // An ecosystem that registered nothing still gets the neutral words.
    assert_eq!(state.lexicon(Ecosystem::Pypi).collection, "project");
}

#[test]
fn test_open_rebuilds_when_the_on_disk_schema_changed() {
    let dir = tempfile::tempdir().unwrap();
    // Leave an index a prior velodex built with a different schema.
    let mut legacy = tantivy::schema::Schema::builder();
    legacy.add_text_field("legacy", tantivy::schema::TEXT);
    tantivy::Index::builder()
        .schema(legacy.build())
        .create_in_dir(dir.path())
        .expect("create the legacy index");
    // Opening discards the mismatched index and rebuilds in place instead of failing startup.
    crate::search::PackageSearch::open(dir.path()).expect("open rebuilds a mismatched index");
}
