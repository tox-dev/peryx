+++
title = "Contributing"
description = "Set up a checkout and run the repository contracts."
sort_by = "weight"
template = "section.html"
weight = 20
+++

Report bugs and open pull requests at [github.com/tox-dev/peryx](https://github.com/tox-dev/peryx).

## Setup

Run setup commands from the repository root:

```shell
rustup show
mise install
prek install
```

`rustup` reads `rust-toolchain.toml`. Mise installs the tools pinned by the repository. Prek installs the commit hooks.

Recipes keep generated state under `.tox/`. Do not put generated files in `src/` or `tests/`.

## Project structure

- `crates/peryx-core` contains stable IDs and ecosystem-neutral values.
- `crates/peryx-driver` defines focused runtime capabilities and serving state.
- `crates/peryx-plugin-registry` validates linked owner registrations and activates owners selected by configuration.
- `crates/peryx-ha` defines availability contracts. `crates/peryx-ha-distributed` implements `dc` and `ha` resources.
- `crates/peryx-storage` persists neutral metadata and content-addressed blobs.
- `crates/peryx-http` and `crates/peryx-web` own shared HTTP and browser boundaries.
- `crates/peryx-ecosystem-*` own one ecosystem's settings, protocols, routes, migrations, tests, fixtures, benchmarks,
  and documentation.
- `crates/peryx-test-support`, `crates/peryx-bench-core`, and `crates/peryx-bench` provide neutral test and benchmark
  infrastructure.
- `crates/peryx` links shipped implementations and owns configuration loading, process startup, and shutdown.

The executable contains all shipped ecosystem and availability implementations. Startup configuration selects which
owners and availability mode to install. An inactive owner registers no runtime capabilities.
`availability.mode = "none"` creates no distributed resource or background task.

See [architecture](@/contributing/architecture.md) and [ecosystem boundaries](@/contributing/ecosystem-boundaries.md)
before moving code between crates.

## Test ownership

Each behavior test belongs to the crate that owns the behavior. A crate must build, test, and reach exact coverage
without another crate's tests.

### Unit tests

Put detailed behavior tests under `crates/CRATE/tests/unit/`. When a test needs crate-private access, mount that file
from the library or binary root with a `#[cfg(test)]`-guarded `#[path]` module. Keep the test body out of `src/`; do not
widen production visibility for a test.

Use table-driven cases for repeated inputs. Use real collaborators. Substitute only clocks, filesystems, networks, and
subprocesses. Wait for a signal or use Tokio virtual time; use timeouts only to bound failure.

### Integration tests

Put public-boundary tests under `crates/CRATE/tests/integration/` or at the root of `crates/CRATE/tests/`. Cargo treats
each file at the `tests/` root as a test target. Exercise the crate through its public API with real stores, routers,
and services. Integration tests cover collaboration between components; they do not repeat unit cases.

Cargo discovers test targets inside the owning package. Do not add `[[test]]` entries that point to another package's
files.

### System tests

A system test starts the `peryx` executable or an external service and observes a public boundary. Neutral process and
availability scenarios live in the `peryx` package. Ecosystem scenarios live in that ecosystem's system package. A
system package declares `package.metadata.peryx-ci.kind = "system"`; CI covers it through the system lanes rather than a
crate contract.

Use system tests for process lifecycle, client compatibility, failover, and service faults. Keep parsing and state
transition cases in unit tests.

Run the test policy checks after changing test infrastructure:

```shell
just test-layout
just test-timing
just test-processes
```

`test-layout` rejects test bodies in production source. `test-timing` rejects blind sleeps and polling. `test-processes`
requires ownership and cleanup for spawned tasks, servers, and child processes.

## Local validation

`just --list` describes the public recipes. Common gates are:

- `just test`: crate tests and benchmark harnesses
- `just system-test`: client, ecosystem, availability, simulation, and browser tests
- `just crate-contract PACKAGE .tox/crate-contracts/PACKAGE`: one crate's build, tests, targets, and exact coverage
- `just coverage-native`: all non-system crate contracts
- `just coverage`: complete Linux coverage and report merge
- `just lint`: source, documentation, automation, dependency, and contract lint lanes
- `just docs`: staged owner documentation and site build
- `just site-links`: site build and external-link check
- `just pre-commit`: all repository hooks
- `just all`: lint, complete Linux coverage, docs, and CI-safe hooks
- `just ci`: run `just all` in the Linux Compose service from macOS, Windows, or Linux

Nextest provides process isolation used by several suites. Use the recipes instead of substituting raw `cargo test` for
repository validation.

## Coverage contracts

Each non-system workspace crate has a coverage contract:

```shell
just crate-contract peryx-core .tox/crate-contracts/peryx-core
```

The contract checks applicable library, binary, example, doctest, integration-test, and benchmark targets. It then
requires 100% executable line and function coverage for each declared source root, including test source. Run all
non-system contracts with:

```shell
just coverage-native .tox/coverage/native.lcov
```

System, frontend, and owner fuzz packages produce separate reports. `just coverage-merge` verifies their provenance,
ownership, input digests, and policy digests before enforcing exact workspace and package coverage. A merged total
cannot hide a package shortfall.

Test observable behavior for each executable path. A coverage exclusion requires a path that the language or target
makes impossible to execute, with the reason beside the exclusion.

## CI structure

GitHub Actions delegates checks to the same `just` recipes and repository scripts used locally. Workflow YAML owns event
filters, runner setup, caches, matrices, artifacts, and job dependencies.

- Planning classifies changed paths and balances eight crate-contract shards from recorded timings.
- Lint jobs separate Rust source, documentation, automation, dependencies, and package contracts.
- Platform jobs compile and test operating-system boundaries.
- Crate-contract shards build, test, and cover the non-system packages selected by the plan.
- System jobs cover client, storage, availability, and simulation boundaries; the frontend job covers native and Wasm
  code plus browser behavior.
- Owner fuzz jobs contribute coverage reports. Pull requests mutate up to 32 changed production candidates across eight
  jobs, sampling larger sets with deterministic round-robin shards.
- The coverage job verifies and merges lane reports. The documentation job builds the staged site.
- The gate reads job results and rejects a missing, skipped, cancelled, or failed required lane.

Scheduled jobs run full mutation across eight shards, plus minimum-dependency, Miri, and sanitizer checks.

Run the recipe named by a failed job before changing workflow YAML. Change the workflow only when the fault concerns CI
orchestration.

## Linux through Compose

The repository uses one `compose.yaml`. Services bind-mount the checkout at `/workspace`; builds do not copy the source.
Cargo state, target files, browser state, temporary files, and nested Docker data stay under `.tox/` on the host.

- `just linux RECIPE [ARGS]`: lightweight Linux service with an 8 GiB limit
- `just linux-analysis RECIPE [ARGS]`: Linux analysis profile
- `just linux-system RECIPE [ARGS]`: Linux service plus nested Docker for system tests
- `just linux-16g RECIPE [ARGS]`: lightweight service with a 16 GiB limit
- `just linux-system-16g RECIPE [ARGS]`: system services with a combined 16 GiB limit

Examples:

```shell
just linux crate-contract peryx-core .tox/crate-contracts/peryx-core
just linux-system availability
just linux-system coverage
```

Use the 16 GiB profile after an 8 GiB run records memory pressure. `just compose-check` validates all profiles.
`just clean` removes transient project state. `just clean-all` also removes reusable coverage, browser, fuzz, benchmark,
and nested Docker state.

## Documentation ownership

Shared architecture and configuration belong under `site/content/core/`. Contributor policy belongs under
`site/content/contributing/`. Ecosystem protocol, command, and configuration details belong under
`crates/peryx-ecosystem-NAME/docs/`.

```shell
just site-stage
site/scripts/dev.sh
```

Format Markdown with `prek run mdformat --all-files`, then run `just lint-docs`, `just docs`, and `just site-links`. The
hook installs the repository's Markdown extensions, including GFM tables.

## Change discipline

- Use an imperative commit subject of at most 50 characters without a period.
- Comments and commit bodies explain non-obvious decisions.
- Fix formatter and linter errors instead of suppressing them.
- Run `just pre-commit` before pushing.
