use std::collections::HashMap;

use crate::ecosystem::Ecosystem;

/// Registered terms keep shared surfaces independent of ecosystem IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexicon {
    pub repository: &'static str,
    pub resource: &'static str,
    pub resources: &'static str,
    pub resource_kind: &'static str,
    pub group: &'static str,
    pub groups: &'static str,
    pub artifact: &'static str,
    pub artifacts: &'static str,
    pub read: &'static str,
    pub write: &'static str,
}

impl Lexicon {
    pub const NEUTRAL: Self = Self {
        repository: "repository",
        resource: "resource",
        resources: "resources",
        resource_kind: "resource",
        group: "group",
        groups: "groups",
        artifact: "artifact",
        artifacts: "artifacts",
        read: "read",
        write: "write",
    };
}

#[derive(Debug, Default)]
pub struct LexiconRegistry(HashMap<Ecosystem, &'static Lexicon>);

impl LexiconRegistry {
    pub fn register(&mut self, ecosystem: Ecosystem, lexicon: &'static Lexicon) {
        self.0.insert(ecosystem, lexicon);
    }

    #[must_use]
    pub fn get(&self, ecosystem: &Ecosystem) -> &'static Lexicon {
        self.0.get(ecosystem).copied().unwrap_or(&Lexicon::NEUTRAL)
    }
}

#[cfg(test)]
#[path = "../tests/unit/lexicon/tests.rs"]
mod tests;
