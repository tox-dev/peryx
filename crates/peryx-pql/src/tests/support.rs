//! Shared fixtures: a couple of domain schemas and an in-memory data source.

use std::collections::BTreeSet;
use std::sync::Mutex;

use crate::catalog::{Column, DomainAuth, DomainSchema, FieldClass, Indexability};
use crate::error::PqlError;
use crate::scope::{QueryScope, RepoScope};
use crate::source::{DataSource, FetchFilter};
use crate::value::{Row, Value, ValueType};

pub const DOMAIN: &str = "policy.decisions";
pub const BIG_DOMAIN: &str = "big";
pub const KEYLESS_DOMAIN: &str = "notes";
pub const USAGE_DOMAIN: &str = "usage";
pub const USAGE_SCAN_DOMAIN: &str = "usage_scan";

#[must_use]
fn usage_columns(project: Indexability) -> Vec<Column> {
    vec![
        Column::new(
            "repository",
            ValueType::Str,
            FieldClass::Repository,
            Indexability::Indexed,
            false,
        ),
        Column::new("project", ValueType::Str, FieldClass::Repository, project, false),
        Column::new("hits", ValueType::Int, FieldClass::Repository, Indexability::Scan, true),
        Column::new("bytes", ValueType::Int, FieldClass::Operator, Indexability::Scan, true),
    ]
}

#[must_use]
pub fn usage_schema() -> DomainSchema {
    DomainSchema {
        name: USAGE_DOMAIN,
        columns: usage_columns(Indexability::Indexed),
        auth: DomainAuth::RepositoryOrOperator,
        natural_order: "hits",
        bounded: true,
    }
}

#[must_use]
pub fn usage_scan_schema() -> DomainSchema {
    DomainSchema {
        name: USAGE_SCAN_DOMAIN,
        columns: usage_columns(Indexability::Scan),
        auth: DomainAuth::RepositoryOrOperator,
        natural_order: "hits",
        bounded: true,
    }
}

#[must_use]
pub fn usage_rows() -> Vec<Row> {
    [
        ("pypi", "numpy", 100, 10),
        ("pypi", "scipy", 50, 5),
        ("other", "django", 30, 3),
    ]
    .into_iter()
    .map(|(repository, project, hits, bytes)| {
        Row::new()
            .with("repository", Value::Str(repository.to_owned()))
            .with("project", Value::Str(project.to_owned()))
            .with("hits", Value::Int(hits))
            .with("bytes", Value::Int(bytes))
    })
    .collect()
}

#[must_use]
pub fn keyless_schema() -> DomainSchema {
    DomainSchema {
        name: KEYLESS_DOMAIN,
        columns: vec![
            Column::new(
                "id",
                ValueType::Int,
                FieldClass::Operator,
                Indexability::KeyOrdered,
                true,
            ),
            Column::new("body", ValueType::Str, FieldClass::Operator, Indexability::Scan, false),
        ],
        auth: DomainAuth::OperatorOnly,
        natural_order: "id",
        bounded: true,
    }
}

#[must_use]
pub fn schema() -> DomainSchema {
    DomainSchema {
        name: DOMAIN,
        columns: vec![
            Column::new(
                "repository",
                ValueType::Str,
                FieldClass::Repository,
                Indexability::KeyOrdered,
                false,
            ),
            Column::new(
                "project",
                ValueType::Str,
                FieldClass::Repository,
                Indexability::Indexed,
                false,
            ),
            Column::new(
                "state",
                ValueType::Str,
                FieldClass::Repository,
                Indexability::Scan,
                false,
            ),
            Column::new(
                "source",
                ValueType::Str,
                FieldClass::Operator,
                Indexability::Scan,
                false,
            ),
            Column::new(
                "downloads",
                ValueType::Int,
                FieldClass::Repository,
                Indexability::Scan,
                true,
            ),
            Column::new(
                "blocked",
                ValueType::Bool,
                FieldClass::Repository,
                Indexability::Scan,
                false,
            ),
            Column::new(
                "evaluated_at",
                ValueType::Timestamp,
                FieldClass::Repository,
                Indexability::KeyOrdered,
                true,
            ),
        ],
        auth: DomainAuth::RepositoryOrOperator,
        natural_order: "evaluated_at",
        bounded: true,
    }
}

#[must_use]
pub fn big_schema() -> DomainSchema {
    DomainSchema {
        name: BIG_DOMAIN,
        columns: vec![
            Column::new(
                "repository",
                ValueType::Str,
                FieldClass::Repository,
                Indexability::KeyOrdered,
                false,
            ),
            Column::new("name", ValueType::Str, FieldClass::Public, Indexability::Scan, false),
        ],
        auth: DomainAuth::RepositoryOrOperator,
        natural_order: "name",
        bounded: false,
    }
}

#[must_use]
pub fn operator_scope() -> QueryScope {
    QueryScope::new(RepoScope::All, "operator".to_owned())
}

#[must_use]
pub fn repository_scope(repository: &str) -> QueryScope {
    let mut set = BTreeSet::new();
    set.insert(repository.to_owned());
    QueryScope::new(RepoScope::Only(set), format!("repo:{repository}"))
}

#[must_use]
pub fn decision(repository: &str, project: &str, state: &str, source: &str, at: i64, downloads: i64) -> Row {
    Row::new()
        .with("repository", Value::Str(repository.to_owned()))
        .with("project", Value::Str(project.to_owned()))
        .with("state", Value::Str(state.to_owned()))
        .with("source", Value::Str(source.to_owned()))
        .with("downloads", Value::Int(downloads))
        .with("blocked", Value::Bool(state == "blocked"))
        .with("evaluated_at", Value::Timestamp(at))
}

/// An in-memory source over a fixed row set. It deliberately does not pre-narrow by scope, so the
/// executor's scope injection is the only thing keeping unauthorized rows out.
pub struct TestSource {
    schema: DomainSchema,
    big: DomainSchema,
    keyless: DomainSchema,
    usage: DomainSchema,
    usage_scan: DomainSchema,
    rows: Vec<Row>,
    fail: bool,
    /// Every `(domain, filter)` the executor asked for, so a test can prove the cost gate's leading
    /// filter reaches the source rather than being applied only in memory.
    fetches: Mutex<Vec<(String, Option<FetchFilter>)>>,
}

impl TestSource {
    #[must_use]
    pub fn new(rows: Vec<Row>) -> Self {
        Self {
            schema: schema(),
            big: big_schema(),
            keyless: keyless_schema(),
            usage: usage_schema(),
            usage_scan: usage_scan_schema(),
            rows,
            fail: false,
            fetches: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn failing() -> Self {
        Self {
            fail: true,
            ..Self::new(Vec::new())
        }
    }

    /// The `(domain, filter)` pairs the executor has fetched, in call order.
    #[must_use]
    pub fn fetches(&self) -> Vec<(String, Option<FetchFilter>)> {
        self.fetches.lock().expect("fetch log is not poisoned").clone()
    }
}

impl DataSource for TestSource {
    fn schema(&self, domain: &str) -> Option<&DomainSchema> {
        match domain {
            DOMAIN => Some(&self.schema),
            BIG_DOMAIN => Some(&self.big),
            KEYLESS_DOMAIN => Some(&self.keyless),
            USAGE_DOMAIN => Some(&self.usage),
            USAGE_SCAN_DOMAIN => Some(&self.usage_scan),
            _ => None,
        }
    }

    fn fetch(&self, domain: &str, _scope: &QueryScope, filter: Option<&FetchFilter>) -> Result<Vec<Row>, PqlError> {
        self.fetches
            .lock()
            .expect("fetch log is not poisoned")
            .push((domain.to_owned(), filter.cloned()));
        if self.fail {
            return Err(PqlError::Backend("store down".to_owned()));
        }
        match domain {
            BIG_DOMAIN => Ok(vec![
                Row::new()
                    .with("repository", Value::Str("pypi".to_owned()))
                    .with("name", Value::Str("numpy".to_owned())),
            ]),
            KEYLESS_DOMAIN => Ok(vec![
                Row::new()
                    .with("id", Value::Int(2))
                    .with("body", Value::Str("two".to_owned())),
                Row::new()
                    .with("id", Value::Int(1))
                    .with("body", Value::Str("one".to_owned())),
            ]),
            USAGE_DOMAIN | USAGE_SCAN_DOMAIN => Ok(usage_rows()),
            _ => Ok(self.rows.clone()),
        }
    }
}
