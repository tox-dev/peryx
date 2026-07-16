use clap::Parser as _;

use crate::cli::Cli;

mod cache_tests;
mod index_tests;
mod job_tests;
mod maintenance_tests;
mod mirror_tests;
mod parse_tests;
mod snippet_tests;

pub(super) fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).unwrap()
}
