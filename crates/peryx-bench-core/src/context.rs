use std::path::{Path, PathBuf};

use crate::report::{Table, publish_to};

#[derive(Clone, Debug)]
pub struct BenchmarkContext {
    peryx_binary: PathBuf,
    report_path: PathBuf,
}

impl BenchmarkContext {
    #[must_use]
    pub const fn new(peryx_binary: PathBuf, report_path: PathBuf) -> Self {
        Self {
            peryx_binary,
            report_path,
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

    /// # Errors
    /// Returns an error when the report cannot be read or written.
    pub fn publish(&self, name: &str, table: Table) -> anyhow::Result<()> {
        publish_to(&self.report_path, name, table)
    }
}
