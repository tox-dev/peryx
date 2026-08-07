pub mod images;
pub mod servers;
pub mod workloads;

#[derive(Debug, Clone, Default, clap::Args)]
pub struct Options {
    /// Use a local pull-through mirror to avoid remote rate limits and network variance.
    #[arg(long)]
    mirror: bool,
}

/// Run the OCI suite: every workload not in `skip`, against every registry named in `only`.
///
/// Parts, any of which `--skip` leaves out by name: `pull`, `throughput`, `parallel`, `endpoints`. Set
/// `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN` to pull authenticated (a higher rate ceiling than the
/// anonymous 100/hour); every proxy and crane pick them up.
///
/// With `mirror`, a local pull-through cache stands in for Docker Hub so a many-round run never
/// exhausts the hourly pull ceiling and every server is shielded from upstream network variance. It
/// prices proxy overhead rather than a real Docker Hub fetch, so it is the reproducible-serving
/// variant; the default run talks to Docker Hub directly and should keep `rounds` small.
///
/// # Errors
/// Returns an error when a registry cannot start or a workload against a healthy one fails.
pub async fn run(
    options: &Options,
    rounds: usize,
    skip: &[String],
    only: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    let _mirror = if options.mirror {
        Some(servers::start_mirror(http).await?)
    } else {
        None
    };
    servers::login_crane()?;
    let servers: Vec<_> = servers::all()
        .into_iter()
        .filter(|server| only.is_empty() || only.split(',').any(|name| name == server.name))
        .collect();
    let enabled = |part: &str| !skip.iter().any(|skipped| skipped.eq_ignore_ascii_case(part));
    if enabled("pull") {
        workloads::pulls(&servers, rounds, http).await?;
    }
    if enabled("throughput") {
        workloads::throughput(&servers, rounds, http).await?;
    }
    if enabled("parallel") {
        workloads::fleet(&servers, rounds, http).await?;
    }
    if enabled("endpoints") {
        workloads::endpoints(&servers, rounds, http).await?;
    }
    Ok(())
}
