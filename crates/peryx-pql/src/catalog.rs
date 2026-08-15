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
}
