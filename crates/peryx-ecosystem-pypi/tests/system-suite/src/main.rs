use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
    match peryx::process::run(peryx::cli::Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            ExitCode::FAILURE
        }
    }
}
