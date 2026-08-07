//! The typed value and row model the evaluator operates over.
//!
//! Values are the closed set the language can express: a boolean, a signed integer, a string, or a
//! timestamp (held as whole seconds since the Unix epoch). There is deliberately no floating point,
//! no arbitrary object, and no byte string - the surface stays small so comparison is total and
//! cost is predictable.

use std::cmp::Ordering;

use serde_json::Value as Json;

/// The declared type of a column or literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Bool,
    Int,
    Str,
    Timestamp,
}

impl ValueType {
    /// The lowercase name used in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Str => "string",
            Self::Timestamp => "timestamp",
        }
    }
}

/// A concrete value flowing through the evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    /// Whole seconds since the Unix epoch.
    Timestamp(i64),
}

impl Value {
    /// The value's declared type, or `None` for [`Value::Null`] which carries no type.
    #[must_use]
    pub const fn value_type(&self) -> Option<ValueType> {
        match self {
            Self::Null => None,
            Self::Bool(_) => Some(ValueType::Bool),
            Self::Int(_) => Some(ValueType::Int),
            Self::Str(_) => Some(ValueType::Str),
            Self::Timestamp(_) => Some(ValueType::Timestamp),
        }
    }

    /// Order two values of the same shape. Returns `None` when either side is null or the two
    /// values are of different kinds, so the evaluator can treat an incomparable pair as unmatched
    /// rather than panicking.
    #[must_use]
    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Bool(left), Self::Bool(right)) => Some(left.cmp(right)),
            (Self::Str(left), Self::Str(right)) => Some(left.cmp(right)),
            (Self::Int(left), Self::Int(right)) | (Self::Timestamp(left), Self::Timestamp(right)) => {
                Some(left.cmp(right))
            }
            _ => None,
        }
    }

    /// The JSON rendering used in results: a timestamp serializes as its Unix-second integer, so a
    /// consumer reads the same numeric shape the typed endpoints already emit.
    #[must_use]
    pub fn to_json(&self) -> Json {
        match self {
            Self::Null => Json::Null,
            Self::Bool(value) => Json::Bool(*value),
            Self::Int(value) | Self::Timestamp(value) => Json::from(*value),
            Self::Str(value) => Json::String(value.clone()),
        }
    }
}

/// A single result row: an ordered set of named cells keyed by the domain's static column names.
///
/// A missing column reads as [`Value::Null`]; a data source need not populate every declared column
/// on every row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    cells: Vec<(&'static str, Value)>,
}

impl Row {
    /// An empty row.
    #[must_use]
    pub const fn new() -> Self {
        Self { cells: Vec::new() }
    }

    /// Attach a cell, consuming and returning the row so a source can build one fluently.
    #[must_use]
    pub fn with(mut self, column: &'static str, value: Value) -> Self {
        self.cells.push((column, value));
        self
    }

    /// Read a column, or [`Value::Null`] when the row does not carry it.
    #[must_use]
    pub fn get(&self, column: &str) -> Value {
        self.cells
            .iter()
            .find_map(|(name, value)| (*name == column).then(|| value.clone()))
            .unwrap_or(Value::Null)
    }
}
