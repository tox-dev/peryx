use clap::{Parser as _, Subcommand};

#[derive(clap::Parser)]
struct Fixture {
    #[command(subcommand)]
    command: FixtureCommand,
}

#[derive(Subcommand)]
enum FixtureCommand {
    Serve(peryx::cli::RuntimeArgs),
    #[cfg(feature = "self-update")]
    SelfUpdate,
}

fn main() -> anyhow::Result<()> {
    match Fixture::parse().command {
        FixtureCommand::Serve(args) => peryx::process::run(peryx::cli::Cli {
            command: peryx::cli::Command::Serve(args),
        }),
        #[cfg(feature = "self-update")]
        FixtureCommand::SelfUpdate => peryx::process::run(peryx::cli::Cli {
            command: peryx::cli::Command::SelfManage(peryx::cli::SelfCommand::Update),
        }),
    }
}
