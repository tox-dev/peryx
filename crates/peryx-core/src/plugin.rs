use crate::Ecosystem;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultIndex {
    pub name: &'static str,
    pub route: &'static str,
    pub ecosystem: Ecosystem,
    pub kind: DefaultIndexKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultIndexKind {
    Cached {
        upstream: &'static str,
    },
    Hosted,
    Virtual {
        layers: &'static [&'static str],
        write_target: &'static str,
    },
}
