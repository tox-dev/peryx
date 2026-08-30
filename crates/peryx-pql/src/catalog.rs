use std::collections::BTreeSet;

use crate::value::ValueType;

/// The least authority a caller needs to read a column, ordered from most open to most restricted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FieldClass {
    Public,
    Repository,
    Operator,
    Administrator,
}

impl FieldClass {
    #[must_use]
    pub fn most_restrictive(self, other: Self) -> Self {
        self.max(other)
    }
}

/// The field classes a caller may read. The authorization boundary decides membership, so the
/// authority lattice keeps one definition and the query layer only asks whether a column is in the
/// set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldVisibility {
    classes: BTreeSet<FieldClass>,
}

impl FieldVisibility {
    #[must_use]
    pub fn new(classes: impl IntoIterator<Item = FieldClass>) -> Self {
        Self {
            classes: classes.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn permits(&self, class: FieldClass) -> bool {
        self.classes.contains(&class)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indexability {
    KeyOrdered,
    Indexed,
    Scan,
}

impl Indexability {
    #[must_use]
    pub const fn is_cheap(self) -> bool {
        matches!(self, Self::KeyOrdered | Self::Indexed)
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: &'static str,
    pub value_type: ValueType,
    pub class: FieldClass,
    pub indexability: Indexability,
    pub numeric: bool,
}

impl Column {
    #[must_use]
    pub const fn new(
        name: &'static str,
        value_type: ValueType,
        class: FieldClass,
        indexability: Indexability,
        numeric: bool,
    ) -> Self {
        Self {
            name,
            value_type,
            class,
            indexability,
            numeric,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainAuth {
    RepositoryOrOperator,
    OperatorOnly,
    Administration,
}

#[derive(Debug, Clone)]
pub struct DomainSchema {
    pub name: &'static str,
    pub columns: Vec<Column>,
    pub auth: DomainAuth,
    pub natural_order: &'static str,
    /// Bounded domains need no indexed leading predicate.
    pub bounded: bool,
    pub pushdown: &'static [&'static str],
}

impl DomainSchema {
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }

    /// The schema a caller may name. Every clause validates against this projection, so a column
    /// above the caller's class is unknown rather than filtered once its value has already shaped
    /// the page.
    #[must_use]
    pub fn visible_to(&self, visibility: &FieldVisibility) -> Self {
        Self {
            columns: self
                .columns
                .iter()
                .filter(|column| visibility.permits(column.class))
                .cloned()
                .collect(),
            ..self.clone()
        }
    }
}
