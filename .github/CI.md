# Continuous integration

GitHub Actions installs tools, selects work, and calls repository commands. The `justfile` and scripts own validation
behavior, coverage policy, deadlines, and diagnostics. Workflow YAML owns events, permissions, runners, caches,
artifacts, matrices, and dependencies.

Keep test logic out of workflow YAML. Add or change a public `just` recipe first, run it from the checkout, then call it
from the workflow.

## Pull request graph

1. `changes` classifies paths and creates eight crate-contract shards. Prior timings set package weights; source size
   supplies the initial estimate.
1. `lint-source`, `lint-docs`, `lint-automation`, `lint-deps`, `lint-contracts`, `platform-test`, `crate-contracts`,
   `system`, `frontend`, owner fuzz jobs, `mutation-diff`, and `docs` run from that classification.
1. `coverage` downloads crate, system, frontend, and selected fuzz reports. It verifies their contracts before merging
   them.
1. `ci-gate` checks each required producer result against the event policy. Branch protection uses this stable job
   instead of matrix-generated names.

Producer matrices set `fail-fast: false`, so one failed package or platform does not hide another failure. Scheduled
jobs add full mutation, Miri, sanitizers, dependency lower-bound checks, and other long-running analysis. CodSpeed and
external conformance use separate workflows with their own change selection and final gates.

## Crate and system ownership

Each package without `package.metadata.peryx-ci.kind = "system"` enters the crate-contract matrix. One contract:

- builds all targets with all features;
- runs applicable doctests, libraries, binaries, examples, integration tests, and benchmarks;
- inventories the package's source roots;
- requires 100% executable line and function coverage, including test source;
- writes an LCOV report, source digest, policy digest, and timing record.

A crate must satisfy that contract without coverage from another crate's tests.

Metadata-declared system packages start the executable or an external service. They contribute coverage through two
lanes:

- `coverage-system-clients`: ecosystem, package-client, and external-client boundaries
- `coverage-system-distributed`: availability, replication, and simulation boundaries

`coverage-frontend` produces native, Wasm, and merged browser reports. Owner package metadata selects fuzz, conformance,
and CodSpeed targets. The aggregate coverage job rejects missing reports, stale source digests, mismatched policy
digests, unowned sources, and any line or function shortfall.

## Local entry points

Run the public recipe that matches the CI lane:

```console
just lint-source
just lint-docs
just lint-automation
just lint-deps
just lint-contracts origin/main
just crate-contract peryx-core .tox/crate-contracts/peryx-core
just coverage-system-clients .tox/coverage/system-clients.lcov
just coverage-system-distributed .tox/coverage/system-distributed.lcov
just coverage-frontend .tox/coverage/frontend-native.lcov .tox/coverage/frontend-wasm.lcov .tox/coverage/frontend.lcov
just coverage
just docs
just pre-commit-ci
```

`just test` runs workspace tests and benchmark harnesses. `just system-test` runs all default system lanes without
coverage. `just coverage-native` runs all non-system crate contracts. `just lint` runs all lint lanes. `just all` runs
linting, complete Linux coverage, docs, and CI-safe hooks without repeating uninstrumented tests.

Run `just platform-contract` on each supported host. Use `just conformance`, `just fuzz-package`, `just mutation`, and
`just codspeed` for their selected package or revision. `just --list` documents each argument.

## Linux containers

The single `compose.yaml` bind-mounts the checkout at `/workspace`; it does not copy source into an image. Cargo,
target, temporary, browser, and nested Docker state remains under `.tox/` on the host.

- `just linux RECIPE [ARGS]` uses the 8 GiB lightweight test service.
- `just linux-analysis RECIPE [ARGS]` uses the Linux analysis profile.
- `just linux-system RECIPE [ARGS]` adds nested Docker for system tests.
- `just linux-16g RECIPE [ARGS]` uses the 16 GiB lightweight service.
- `just linux-system-16g RECIPE [ARGS]` gives the system service 12 GiB and nested Docker 4 GiB.

Examples:

```console
just linux crate-contract peryx-core .tox/crate-contracts/peryx-core
just linux-system coverage
just linux-system system-test
just linux-system-16g coverage
```

Use a 16 GiB profile after an 8 GiB run records memory pressure. `just compose-check` validates all service definitions.
`just linux-image` rebuilds from the configured upstream images and prints tool versions. `just linux-system-clean`
stops nested Docker services after a system run.

## Reports and diagnostics

Local and CI runs write generated state under `.tox/`. Crate contracts emit LCOV, source contracts, and timings. System,
frontend, and selected fuzz lanes emit LCOV for the aggregate job. CI marks required artifact uploads as errors when a
report is absent.

Use `just ci-diagnostics OUTPUT` to capture runner state. Use `just coverage-clean` to remove Rust coverage builds,
`just clean` for transient state, or `just clean-all` to discard reusable project build state.

## Failure policy

Runner jobs have deadlines. Crate and system matrices continue after an individual leg fails. The coverage job waits for
each required report, and `ci-gate` checks skipped and failed jobs against event and changed-path policy. A required
skip, missing report, stale contract, coverage shortfall, or producer failure fails the pull request gate.
