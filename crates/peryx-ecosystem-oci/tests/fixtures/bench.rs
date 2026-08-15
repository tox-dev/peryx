use clap::Command;
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::servers::http_client;
use peryx_bench_core::suite::BenchmarkRun;
use peryx_ecosystem_oci::bench::BENCHMARK_SUITE;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("credential") => credential().await,
        _ => contract(),
    }
}

#[cfg(unix)]
fn contract() -> anyhow::Result<()> {
    use anyhow::{Context as _, ensure};
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir()?;
    let bin = directory.path().join("bin");
    std::fs::create_dir(&bin)?;
    let crane = bin.join("crane");
    let script = br#"#!/bin/sh
token=$(cat)
[ "$*" = "auth login index.docker.io -u child-user --password-stdin" ] && [ "$token" = child-token ]
"#;
    std::fs::write(&crane, script)?;
    let mut permissions = std::fs::metadata(&crane)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(crane, permissions)?;
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&inherited_path)))?;
    let status = std::process::Command::new(std::env::args_os().next().context("fixture executable")?)
        .arg("credential")
        .env("DOCKERHUB_USERNAME", "child-user")
        .env("DOCKERHUB_TOKEN", "child-token")
        .env("PATH", path)
        .status()?;
    ensure!(status.success(), "credential fixture failed");
    Ok(())
}

#[cfg(not(unix))]
fn contract() -> anyhow::Result<()> {
    anyhow::bail!("the credential fixture requires Unix process scripts")
}

async fn credential() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let skip = ["throughput", "parallel", "endpoints"].map(str::to_owned);
    BENCHMARK_SUITE
        .run(BenchmarkRun {
            context: &BenchmarkContext::new(directory.path().join("peryx"), directory.path().join("report.toml")),
            rounds: 0,
            skip: &skip,
            only: "direct",
            http: &http_client()?,
            matches: &BENCHMARK_SUITE
                .configure(Command::new("fixture"))
                .try_get_matches_from(["fixture"])?,
        })
        .await
}
