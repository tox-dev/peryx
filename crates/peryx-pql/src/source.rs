use crate::catalog::DomainSchema;
use crate::error::PqlError;
use crate::scope::QueryScope;
use crate::value::{Row, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchFilter {
    pub column: &'static str,
    pub values: Vec<Value>,
}

pub trait DataSource: Send + Sync {
    fn schema(&self, domain: &str) -> Option<&DomainSchema>;

    /// Read candidate rows. The executor reapplies scope and predicates before returning results.
    ///
    /// # Errors
    /// Returns [`PqlError::Backend`] when the underlying store cannot answer.
    fn fetch(&self, domain: &str, scope: &QueryScope, filter: Option<&FetchFilter>) -> Result<Vec<Row>, PqlError>;
}
