//! The abstract syntax the wire front-end produces and the evaluator consumes.
//!
//! This module is the seam the wire form is kept separate from: the textual parser in [`crate::parse`]
//! is one producer of an [`Ast`], and a future JSON-AST decoder would produce the very same tree
//! without touching the evaluator. Nothing here is textual — it is the query's meaning, not its
//! spelling.
//!
//! Read-only by construction: there is no mutation node. The grammar can express selection,
//! filtering, ordering, bounded pagination, one declared join, and a fixed set of aggregates, and
//! nothing that writes, deletes, or has a side effect.

/// A parsed query over one domain, optionally joined to a second declared domain.
#[derive(Debug, Clone, PartialEq)]
pub struct Ast {
    pub domain: String,
    pub join: Option<Join>,
    pub predicate: Option<Predicate>,
    pub selection: Selection,
    pub aggregate: Option<Aggregate>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<u32>,
}

/// A bounded, declared join to a second domain on one shared key.
///
/// Both the domain and the key are named explicitly; there is no inferred join graph. Whether the
/// join can be admitted at all is a cost decision made later against the probe side's indexability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub domain: String,
    pub on: String,
}

/// The projected columns: every declared column, or a named subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    All,
    Columns(Vec<String>),
}

/// The `where` predicate, CEL-shaped: boolean logic over comparisons, membership, and prefix match.
/// There is no arithmetic, no function call, and no leading-wildcard match.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Or(Box<Self>, Box<Self>),
    And(Box<Self>, Box<Self>),
    Not(Box<Self>),
    Compare {
        field: String,
        op: CompareOp,
        value: Literal,
    },
    In {
        field: String,
        values: Vec<Literal>,
    },
    StartsWith {
        field: String,
        prefix: Literal,
    },
}

/// The comparison operators, the whole set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    /// The source spelling, for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// A literal, or a named parameter placeholder bound out of band before evaluation.
///
/// A parameter is never spliced into the query text; it arrives as [`Literal::Param`] and is
/// replaced by a concrete literal during binding, so a caller value can never change the query's
/// structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Str(String),
    Int(i64),
    Bool(bool),
    Timestamp(i64),
    Param(String),
}

/// One `order by` term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderKey {
    pub field: String,
    pub descending: bool,
}

/// A declared aggregation: a set of aggregate terms grouped by declared keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aggregate {
    pub terms: Vec<AggregateTerm>,
    pub group_by: Vec<String>,
}

/// One aggregate output: a function over an optional column, exposed under an alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateTerm {
    pub func: AggregateFunc,
    /// `None` only for [`AggregateFunc::Count`], which counts rows rather than a column.
    pub column: Option<String>,
    pub alias: String,
}

/// The fixed, cost-bounded aggregate set. No windowing, no user-defined aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Sum,
    Count,
    Min,
    Max,
}

impl AggregateFunc {
    /// The source spelling, for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Count => "count",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    /// Whether the function requires a numeric column argument. Only `count` does not.
    #[must_use]
    pub const fn needs_column(self) -> bool {
        !matches!(self, Self::Count)
    }
}
