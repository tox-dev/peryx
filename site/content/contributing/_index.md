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
mise install --locked
prek install
```

Use `mise install --locked` to require every resolution in `mise.lock`. `rustup` reads `rust-toolchain.toml`. Prek
installs the commit hooks. Browser recipes select their package-compatible Chrome for Testing binary from the locked
`browser` mise environment.

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

See [build instructions](@/contributing/build.md), [architecture](@/contributing/architecture.md),
[runtime architecture](@/contributing/runtime-architecture.md), [contributor terminology](@/contributing/glossary.md),
and [ecosystem boundaries](@/contributing/ecosystem-boundaries.md) before moving code between crates.

## Test ownership

Each behavior test belongs to the crate that owns the behavior. The workspace coverage run measures every crate through
its owning tests.

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
availability scenarios live in the `peryx` package. Ecosystem scenarios live in that ecosystem's system package. Cargo
discovers these packages as workspace members, so they run through the same workspace test and coverage commands.

Use system tests for process lifecycle, client compatibility, failover, and service faults. Keep parsing and state
transition cases in unit tests.

Run `just test` after changing test infrastructure. Process and server tests must wait for observable readiness and own
cleanup for every spawned task, listener, and child process.

## Local validation

`just --list` describes the public recipes. Common gates are:

- `just test`: crate tests and benchmark harnesses
- `just coverage-native`: complete native workspace coverage
- `just coverage-frontend`: native and Wasm browser coverage
- `just coverage`: complete native and frontend coverage
- `just lint`: source, documentation, automation, dependency, and contract lint lanes
- `just docs`: documentation build and search index
- `just site-links`: external-link check
- `just pre-commit`: all repository hooks
- `just all`: lint, complete coverage, and docs
- `just ci`: run the same complete gate as `just all`

Nextest provides process isolation used by several suites. Use the recipes instead of substituting raw `cargo test` for
repository validation.

## Coverage

The native coverage command compiles every workspace target with all features, runs the hermetic test suite and
benchmark harnesses, and checks the report with cargo-llvm-cov:

```shell
just coverage-native .tox/coverage/native.lcov
```

The command rejects every uncovered workspace source line. Browser coverage combines native and Wasm reports and
requires 100% line coverage.

Test observable behavior for each executable path. A coverage exclusion requires a path that the language or target
makes impossible to execute, with the reason beside the exclusion.

## CI structure

The [CI guide](@/contributing/ci.md) maps each job to its local recipe and documents test synchronization, nightly
analysis, and runner-specific orchestration. Run the recipe named by a failed job before changing workflow YAML.

## Docker-backed tests

Storage and service-fault tests use testcontainers against the host's Docker daemon. Start Docker before running recipes
that declare a Docker dependency, such as `just storage-s3` or `just coverage-native`. The same recipes run directly on
developer machines and GitHub-hosted runners.

`just clean` removes transient project state. `just clean-all` also removes reusable coverage, browser, fuzz, and
benchmark state.

## Dependency updates

Renovate checks Cargo, npm, GitHub Actions, container images, mise, pre-commit hooks, and the Rust toolchain each
Tuesday. A separate weekly update keeps the Playwright and Puppeteer packages aligned with their verified Chrome for
Testing archives and regenerates the Mermaid diagrams. Lock maintenance covers `Cargo.lock`, every `package-lock.json`,
and the mise lockfiles. Update grouped pull requests through their manifests; do not edit lockfiles by hand.

## Documentation ownership

End-user docs cover product behavior and operation, including failures. Put shared guidance under `site/content/core/`
and ecosystem-specific protocols or configuration under `site/content/ecosystems/`. Contributor docs own source-level
concepts such as crate boundaries, Rust APIs, test structure, and internal ownership.

```shell
just site-dev
```

Format Markdown with `prek run mdformat --all-files`. Mermaid sources under `site/diagrams/` render through
`just render-diagrams` with Puppeteer's locked Chrome for Testing revision; `just docs` rejects stale SVGs. Run
`just lint-docs`, `just docs`, and `just site-links` before submitting documentation changes.

## Change discipline

- Use an imperative commit subject of at most 50 characters without a period.
- Comments and commit bodies explain non-obvious decisions.
- Fix formatter and linter errors instead of suppressing them.
- Run `just pre-commit` before pushing.
