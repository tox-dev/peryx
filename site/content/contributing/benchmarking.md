+++
title = "Run the benchmarks"
description = "The peryx-bench harness: comparison runs, per-ecosystem runs, and the A/B regression gate against a base commit."
weight = 5
+++

`peryx-bench` produces the tables the documentation publishes and gates a change against a regression. Every command
below assumes a checked-out repository and a release build. A debug build times unoptimized code, so its numbers say
nothing about peryx.

## Compare peryx against the other tools

The published comparison tables come from this form, which runs every server on one machine, in one run, against the
same workload:

```shell
cargo run --release -p peryx-bench                       # every ecosystem
cargo run --release -p peryx-bench -- --ecosystem pypi   # one ecosystem
cargo run --release -p peryx-bench -- --ecosystem oci
```

## Check a change against a base commit

The A/B form builds both revisions and measures each through this same harness, so the method matches on both sides. It
prints a per-metric verdict aggregated with the geometric mean and gates only the local metrics, since network variance
peryx does not control dominates any row that fetches from a real upstream:

```shell
cargo run --release -p peryx-bench -- --rounds 7 ab <base-commit>
cargo run --release -p peryx-bench -- --rounds 7 ab <base-commit> --head-first
```

Run both orders on the same machine. A result that changes with the order is thermal or background-load drift, not a
regression. Keep the machine on AC power, close other CPU and disk work, and compare the two revisions without changing
the compiler, dependency lockfile, power mode, or host. The harness reports the median, coefficient of variation,
min-max range, and outliers; it excludes a noisy metric from the gate instead of treating it as evidence.

## OCI runs need Docker and a mirror

The OCI benchmarks need a running [Docker](https://www.docker.com/) daemon. Pulling from
[Docker Hub](https://hub.docker.com/) with nothing in front of it, an anonymous account hits the pull ceiling partway
through a comparison, so export `DOCKERHUB_USERNAME` and a read-only
[access token](https://docs.docker.com/security/for-developers/access-tokens/) in `DOCKERHUB_TOKEN`; the harness threads
them into every registry and into [crane](https://github.com/google/go-containerregistry).

Under `--mirror` the harness stands a local pull-through cache in front of Docker Hub and points every registry at it,
so the run is rate-limit-free and repeatable. Without it the cold rows carry the real upstream fetch, so the harness
marks them network-bound and keeps them out of the regression gate:

```shell
cargo run --release -p peryx-bench -- --ecosystem oci --mirror
```

## Run the same CPU benchmarks locally and in CI

GitHub Actions uses CodSpeed's CPU simulation because shared-runner wall time changes with host load and CPU model. The
simulation counts the work performed by the benchmark on a simulated CPU. It runs once, excludes time spent inside
system calls, and is therefore suited to the in-process parser, renderer, and router benchmarks rather than the
end-to-end network workloads above.

The local runner uses the immutable CI image when its Dockerfile definition has been published. The image pins Ubuntu,
Rust, cargo-codspeed, the CodSpeed CLI, and CodSpeed's Valgrind fork; the runner also uses the CI workspace path, thin
LTO, generic glibc CPU routines, and one malloc arena. On an ARM64 host with Docker:

```shell
ci/run-codspeed-local.sh login
ci/run-codspeed-local.sh peryx-ecosystem-pypi
ci/run-codspeed-local.sh peryx-ecosystem-oci
```

`login` is needed once and stores the CodSpeed credential in a Docker volume. Build artifacts use a separate volume
keyed by the image definition. If the current Dockerfile has not been published, the runner builds it locally; compare
those results only with another run using the same definition.

CodSpeed simulation does not measure kernel, filesystem, socket, or upstream latency. Use `peryx-bench` for those paths.
The OCI blob-serving benchmark reads the blob from disk inside each router request, so CodSpeed excludes it from CPU
simulation. It remains available through standard Criterion, along with every CPU benchmark. Host wall-clock results are
valid only on the same quiet machine under the same toolchain and power conditions:

```shell
cargo bench --locked -p peryx-ecosystem-pypi
cargo bench --locked -p peryx-ecosystem-oci
```

The methodology follows [CodSpeed's CPU simulation guidance](https://codspeed.io/docs/instruments/cpu), its
[variance controls](https://codspeed.io/docs/instruments/cpu/reducing-variance),
[Criterion's measurement guidance](https://bheisler.github.io/criterion.rs/book/user_guide/command_line_options.html),
and the
[Google Benchmark interleaving rationale](https://github.com/google/benchmark/blob/main/docs/random_interleaving.md).
