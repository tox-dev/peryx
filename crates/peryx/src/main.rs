use clap::Parser as _;

#[cfg(not(feature = "composition"))]
compile_error!("the peryx binary requires at least one `composition-*` feature");

// PyPI transforms are 13–17% faster than with the system allocator.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    peryx::process::run(peryx::cli::Cli::parse())
}
