+++
title = "Run the benchmarks"
description = "Own, test, and run package benchmarks."
weight = 5
+++

`peryx-bench-core` owns neutral measurements and reports. `peryx-bench` owns neutral execution and comparison. Ecosystem
owners keep workloads, fixtures, command arguments, and result interpretation in their own crates.

## Benchmark contracts

`just test` runs crate tests, then executes all workspace benchmark harnesses. The benchmark lane uses a 20-minute
deadline by default:

```shell
just benchmark
just benchmark 600
```

The argument is a positive timeout in seconds. A timeout fails the recipe rather than leaving a harness running.

Each non-system crate contract compiles and executes that crate's benchmark targets under coverage:

```shell
just crate-contract PACKAGE .tox/crate-contracts/PACKAGE
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

## CodSpeed

CodSpeed runs owner-selected compute benchmarks in the repository container:

```shell
just codspeed PACKAGE
```

Package metadata declares the selected targets, job count, label, and change key. Add selection there instead of adding
package branches to workflow YAML.

Use CodSpeed for in-process compute paths. Use a named benchmark on one quiet host for filesystem, socket, subprocess,
or upstream measurements, then compare revisions under the same host conditions.
