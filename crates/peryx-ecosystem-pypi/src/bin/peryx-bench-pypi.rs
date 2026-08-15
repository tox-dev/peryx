#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match peryx_bench::Runner::system()
        .run_from(peryx_ecosystem_pypi::bench::BENCHMARK_SUITE, std::env::args_os())
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => match error.downcast::<clap::Error>() {
            Ok(error) => error.exit(),
            Err(error) => Err(error),
        },
    }
}
