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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub domain: String,
    pub on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    All,
    Columns(Vec<String>),
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Str(String),
    Int(i64),
    Bool(bool),
    Timestamp(i64),
    Param(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderKey {
    pub field: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aggregate {
    pub terms: Vec<AggregateTerm>,
    pub group_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateTerm {
    pub func: AggregateFunc,
    /// `None` only for [`AggregateFunc::Count`], which counts rows rather than a column.
    pub column: Option<String>,
    pub alias: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Sum,
    Count,
    Min,
    Max,
}

impl AggregateFunc {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Count => "count",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    #[must_use]
    pub const fn needs_column(self) -> bool {
        !matches!(self, Self::Count)
    }
}
