use std::cmp::Ordering;

use serde_json::Value as Json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Bool,
    Int,
    Str,
    Timestamp,
}

impl ValueType {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Timestamp(i64),
}

impl Value {
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    cells: Vec<(&'static str, Value)>,
}

impl Row {
    #[must_use]
    pub const fn new() -> Self {
        Self { cells: Vec::new() }
    }

    #[must_use]
    pub fn with(mut self, column: &'static str, value: Value) -> Self {
        self.cells.push((column, value));
        self
    }

    /// Missing columns read as [`Value::Null`].
    #[must_use]
    pub fn get(&self, column: &str) -> Value {
        self.cells
            .iter()
            .find_map(|(name, value)| (*name == column).then(|| value.clone()))
            .unwrap_or(Value::Null)
    }
}
