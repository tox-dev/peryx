//! The static registry of domains and their columns.
//!
//! A domain is a named, typed relation. Each column declares its type, the authority a caller needs
//! to read it (its [`FieldClass`]), whether it is numeric enough to aggregate, and — the part the
//! cost model depends on — how cheaply the backing source can filter on it ([`Indexability`]). The
//! catalog is what makes validation and field classification static rather than per-row.
//!
//! The classification levels mirror `peryx-http`'s `FieldClassification` one-for-one; the wire layer
//! maps between them so the actual field-drop reuses that existing primitive rather than a parallel
//! implementation here.

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
    /// The more restrictive of two classes, used so a joined column inherits the stricter side.
    #[must_use]
    pub fn most_restrictive(self, other: Self) -> Self {
        self.max(other)
    }
}

/// How cheaply a source can filter or order on a column.
///
/// This is per-source truth, not a neutral guess: a store advertises `KeyOrdered` for the column its
/// key order follows, `Indexed` for a column it maintains a secondary index on, and `Scan` for a
/// column it can only answer by reading rows. The cost gate reads these to admit or refuse a plan,
/// and the join planner requires the probe side's key be at least `Indexed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indexability {
    KeyOrdered,
    Indexed,
    Scan,
}

impl Indexability {
    /// Whether a source can serve this column cheaply enough to lead a filter or bound a join.
    #[must_use]
    pub const fn is_cheap(self) -> bool {
        matches!(self, Self::KeyOrdered | Self::Indexed)
    }
}

/// One declared column.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: &'static str,
    pub value_type: ValueType,
    pub class: FieldClass,
    pub indexability: Indexability,
    /// Whether `sum`/`min`/`max` may target the column.
    pub numeric: bool,
}

impl Column {
    /// Declare a column.
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

/// The authority a whole domain requires before any row is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainAuth {
    /// Readable by a repository-scoped caller for its own repository, or an operator wide.
    RepositoryOrOperator,
    /// Operator-only; an unauthorized caller cannot confirm the domain exists.
    OperatorOnly,
    /// Administrator-only.
    Administration,
}

/// A named, typed relation with its columns and read authority.
#[derive(Debug, Clone)]
pub struct DomainSchema {
    pub name: &'static str,
    pub columns: Vec<Column>,
    pub auth: DomainAuth,
    /// The column whose value orders results by default, newest or largest first.
    pub natural_order: &'static str,
    /// Whether the whole domain is small enough that a full authorized scan is always affordable.
    /// A bounded domain needs no indexed leading predicate; an unbounded one does.
    pub bounded: bool,
}

impl DomainSchema {
    /// Look up a column by name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }
}
