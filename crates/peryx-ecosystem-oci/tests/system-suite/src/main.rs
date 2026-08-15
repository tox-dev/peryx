use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    peryx::process::run(peryx::cli::Cli::parse())
}
