use std::path::{Path, PathBuf};

use crate::report::{Table, publish_to};

#[derive(Clone, Debug)]
pub struct BenchmarkContext {
    peryx_binary: PathBuf,
    report_path: PathBuf,
    scratch: PathBuf,
}

#[cfg(test)]
#[path = "../tests/unit/context.rs"]
mod tests;

impl BenchmarkContext {
    #[must_use]
    pub fn new(peryx_binary: PathBuf, report_path: PathBuf) -> Self {
        Self::with_scratch(peryx_binary, report_path, PathBuf::from(".tox/bench/scratch"))
    }

    #[must_use]
    pub const fn with_scratch(peryx_binary: PathBuf, report_path: PathBuf, scratch: PathBuf) -> Self {
        Self {
            peryx_binary,
            report_path,
            scratch,
        }
    }

    #[must_use]
    pub fn peryx_binary(&self) -> &Path {
        &self.peryx_binary
    }

    #[must_use]
    pub fn report_path(&self) -> &Path {
        &self.report_path
    }

    #[must_use]
    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// # Errors
    /// Returns an error when the report cannot be read or written.
    pub fn publish(&self, name: &str, table: Table) -> anyhow::Result<()> {
        publish_to(&self.report_path, name, table)
    }
}
