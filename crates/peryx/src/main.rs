use clap::Parser as _;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    peryx::process::run(peryx::cli::Cli::parse())
}
