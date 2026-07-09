//! The OCI benchmark suite: the workloads, the competitor registries, and the image fixtures.
//!
//! This mirrors the `peryx-ecosystem-oci` crate and the site's `content/ecosystems/oci` family.
//! Every competitor runs as a pull-through cache of Docker Hub, the registry analogue of a caching
//! `PyPI` mirror, so the tables read against the same `direct` baseline: talking to Docker Hub with
//! no proxy in between.

pub mod images;
pub mod servers;
pub mod workloads;

/// Run the OCI suite: every workload not in `skip`, against every registry named in `only`.
///
/// Parts, any of which `--skip` leaves out by name: `pull`, `throughput`, `parallel`. Set
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
    rounds: usize,
    mirror: bool,
    skip: &[String],
    only: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    let _mirror = if mirror {
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
    Ok(())
}
