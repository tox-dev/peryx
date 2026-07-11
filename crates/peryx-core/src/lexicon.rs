//! Each ecosystem's user-facing words for peryx's neutral concepts, and the registry surfaces look
//! them up in.

use std::collections::HashMap;

use crate::ecosystem::Ecosystem;

/// One ecosystem's user-facing words for peryx's neutral concepts. The neutral name is in each
/// field's doc comment; the value is what a user of that ecosystem calls the same thing.
///
/// peryx's internal model is one neutral vocabulary, but a reader arriving from Docker thinks
/// "registry, repository, tag, blob, push, pull". Each ecosystem crate defines its own `Lexicon`
/// value (this crate stays neutral and never names an ecosystem's words) and registers it, so
/// user-facing surfaces localize a label by looking it up for an index's ecosystem rather than
/// branching on the ecosystem to pick a word. [`Lexicon::NEUTRAL`] is the fallback before any
/// ecosystem registers one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexicon {
    /// The endpoint a client points at (neutral: index).
    pub server: &'static str,
    /// One named collection of releases (neutral: project).
    pub collection: &'static str,
    /// Many collections (neutral: projects).
    pub collections: &'static str,
    /// The search noun for a collection (neutral: package).
    pub search_noun: &'static str,
    /// One release of a collection (neutral: version).
    pub release: &'static str,
    /// Many releases (neutral: versions).
    pub releases: &'static str,
    /// One stored downloadable piece (neutral: file).
    pub artifact: &'static str,
    /// Many pieces (neutral: files).
    pub artifacts: &'static str,
    /// The verb for fetching (neutral: download).
    pub get: &'static str,
    /// The verb for storing your own (neutral: upload).
    pub put: &'static str,
}

impl Lexicon {
    /// peryx's own neutral vocabulary, used until an ecosystem registers its own words.
    pub const NEUTRAL: Self = Self {
        server: "index",
        collection: "project",
        collections: "projects",
        search_noun: "package",
        release: "version",
        releases: "versions",
        artifact: "file",
        artifacts: "files",
        get: "download",
        put: "upload",
    };
}

/// The installed ecosystems' vocabularies, filled by each driver at startup.
///
/// A surface localizes a label by looking up an index's ecosystem instead of matching on it, so
/// nothing outside an ecosystem crate names that ecosystem's words.
#[derive(Debug, Default)]
pub struct LexiconRegistry(HashMap<Ecosystem, &'static Lexicon>);

impl LexiconRegistry {
    /// Register an ecosystem's vocabulary; its driver calls this at install time.
    pub fn register(&mut self, ecosystem: Ecosystem, lexicon: &'static Lexicon) {
        self.0.insert(ecosystem, lexicon);
    }

    /// The vocabulary for `ecosystem`, or peryx's neutral words when none is registered.
    #[must_use]
    pub fn get(&self, ecosystem: Ecosystem) -> &'static Lexicon {
        self.0.get(&ecosystem).copied().unwrap_or(&Lexicon::NEUTRAL)
    }
}

#[cfg(test)]
mod tests {
    use super::{Lexicon, LexiconRegistry};
    use crate::ecosystem::Ecosystem;

    const DOCKER: Lexicon = Lexicon {
        server: "registry",
        collection: "repository",
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
        assert_eq!(registry.get(Ecosystem::Oci).server, "index");
    }

    #[test]
    fn test_registry_returns_the_registered_lexicon() {
        let mut registry = LexiconRegistry::default();
        registry.register(Ecosystem::Oci, &DOCKER);
        assert_eq!(registry.get(Ecosystem::Oci).collection, "repository");
        assert_eq!(registry.get(Ecosystem::Pypi).collection, "project");
    }
}
