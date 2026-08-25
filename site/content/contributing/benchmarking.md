+++
title = "Run the benchmarks"
description = "Own, test, and run package benchmarks."
weight = 5
+++

`peryx-bench-core` owns neutral measurements and reports. `peryx-bench` owns neutral execution and comparison. Ecosystem
owners keep workloads, fixtures, command arguments, and result interpretation in their own crates.

The PyPI workload and server adapters live in `crates/peryx-ecosystem-pypi/src/bench/`. The OCI workload lives in
`crates/peryx-ecosystem-oci/src/bench/`. Each owner exposes its workload through a `peryx-bench-*` binary.

## Benchmark contracts

`just test` runs crate tests, then executes all workspace benchmark harnesses:

```shell
just benchmark
```

A benchmark target must fail when setup or the measured path fails. Keep behavior assertions in the crate's `tests/`
tree; measurement code belongs in `benches/`. A benchmark does not replace a unit or integration test.

## Run one target

List targets without measuring them:

```shell
cargo bench --workspace --all-features --no-run
```

Execute one harness through the same Cargo mode used by the repository gate:

```shell
cargo test --locked -p PACKAGE --all-features --bench TARGET -- --help
```

Replace `--help` with the target's documented arguments. Keep the toolchain and `Cargo.lock` fixed between revisions.
Record the host, power mode, scratch filesystem, and competing load with measured results. Wait for service readiness
during setup. A pacing sleep is valid when elapsed time is part of the workload.

## Ecosystem suites

Inspect an ecosystem benchmark command or run one target:

```shell
cargo run -p peryx-ecosystem-pypi --features bench --bin peryx-bench-pypi -- --help
cargo bench --locked -p peryx-ecosystem-pypi --bench parse
cargo run -p peryx-ecosystem-oci --features bench --bin peryx-bench-oci -- --help
cargo bench --locked -p peryx-ecosystem-oci --bench manifest_by_digest
```

The PyPI root-catalog fixture is `crates/peryx-ecosystem-pypi/src/bench/packages.rs`. Build its million-project example
before collecting at least five rounds from one machine and power state:

```shell
CARGO_TARGET_DIR=.tox/target-catalog cargo build --release \
  -p peryx-ecosystem-pypi --example catalog_million

/usr/bin/time -l .tox/target-catalog/release/examples/catalog_million
```

## CodSpeed

CodSpeed runs owner-selected benchmarks on its stable bare-metal runners:

```shell
just codspeed PACKAGE MODE
```

Package metadata declares the selected targets, job count, label, and change key. Add selection there instead of adding
package branches to workflow YAML.

Use `simulation` for in-process compute paths and `walltime` for filesystem or socket paths. Compare walltime revisions
under the same host conditions; CI uses CodSpeed's bare-metal runners for that reason.
