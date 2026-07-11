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
cargo run --release -p peryx-bench -- ab <base-commit>
```

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

## Price one request, per ecosystem

The runs above time a whole client against a real network. The [criterion](https://github.com/bheisler/criterion.rs)
suites price a single request served in process through the full router, with no socket and no upstream, from a warm
store:

```shell
cargo bench -p peryx-ecosystem-pypi --bench operations
cargo bench -p peryx-ecosystem-oci --bench operations
```
