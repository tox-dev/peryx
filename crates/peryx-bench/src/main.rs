use clap::Parser as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    peryx_bench::Runner::system().run(peryx_bench::Cli::parse()).await
}
