use clap::{ArgMatches, Command};

use crate::context::BenchmarkContext;

pub struct BenchmarkRun<'a> {
    pub context: &'a BenchmarkContext,
    pub rounds: usize,
    pub skip: &'a [String],
    pub only: &'a str,
    pub http: &'a reqwest::Client,
    pub matches: &'a ArgMatches,
}

#[async_trait::async_trait]
pub trait BenchmarkSuite: Send + Sync {
    fn name(&self) -> &'static str;

    fn configure(&self, command: Command) -> Command;

    async fn run(&self, run: BenchmarkRun<'_>) -> anyhow::Result<()>;
}

#[cfg(test)]
#[path = "../tests/unit/suite.rs"]
mod tests;
