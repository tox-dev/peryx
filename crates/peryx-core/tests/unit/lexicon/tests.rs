use super::{Lexicon, LexiconRegistry};
use crate::ecosystem::Ecosystem;

const ALTERNATE: Lexicon = Lexicon {
    server: "catalog",
    collection: "component",
    ..Lexicon::NEUTRAL
};

#[test]
fn test_neutral_lexicon_is_peryxs_own_words() {
    let neutral = Lexicon::NEUTRAL;
    assert_eq!(
        (neutral.server, neutral.collection, neutral.release),
        ("index", "project", "version")
    );
    assert_eq!(
        (neutral.artifact, neutral.get, neutral.put),
        ("file", "download", "upload")
    );
}

#[test]
fn test_registry_falls_back_to_the_neutral_lexicon() {
    let registry = LexiconRegistry::default();
    assert_eq!(registry.get(Ecosystem::new("other")).server, "index");
}

#[test]
fn test_registry_returns_the_registered_lexicon() {
    let mut registry = LexiconRegistry::default();
    registry.register(Ecosystem::new("other"), &ALTERNATE);
    assert_eq!(registry.get(Ecosystem::new("other")).collection, "component");
    assert_eq!(registry.get(Ecosystem::new("example")).collection, "project");
}
