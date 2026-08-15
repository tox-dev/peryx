use super::{Lexicon, LexiconRegistry};
use crate::ecosystem::Ecosystem;

const ALTERNATE: Lexicon = Lexicon {
    repository: "catalog",
    resource: "component",
    ..Lexicon::NEUTRAL
};

#[test]
fn test_neutral_lexicon_is_peryxs_own_words() {
    let neutral = Lexicon::NEUTRAL;
    assert_eq!(
        (neutral.repository, neutral.resource, neutral.group),
        ("repository", "resource", "group")
    );
    assert_eq!(
        (neutral.artifact, neutral.read, neutral.write),
        ("artifact", "read", "write")
    );
}

#[test]
fn test_registry_falls_back_to_the_neutral_lexicon() {
    let registry = LexiconRegistry::default();
    assert_eq!(registry.get(&Ecosystem::new("other")).repository, "repository");
}

#[test]
fn test_registry_returns_the_registered_lexicon() {
    let mut registry = LexiconRegistry::default();
    registry.register(Ecosystem::new("other"), &ALTERNATE);
    assert_eq!(registry.get(&Ecosystem::new("other")).resource, "component");
    assert_eq!(registry.get(&Ecosystem::new("example")).resource, "resource");
}
