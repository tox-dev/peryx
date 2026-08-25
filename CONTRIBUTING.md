# Contributing to peryx

Install the repository toolchain:

```shell
rustup show
mise install
prek install
```

`rustup` reads `rust-toolchain.toml`. Mise installs the remaining tools, and Prek installs the commit hooks. The
[contributor guide](https://peryx.readthedocs.io/en/latest/contributing/) lists focused commands and their
prerequisites.

## Repository structure

- `peryx-core`, `peryx-driver`, `peryx-storage`, `peryx-http`, and `peryx-web` own ecosystem-neutral contracts,
  coordination, persistence, and protocol boundaries.
- `peryx-plugin-registry` validates linked ecosystem registrations and activates the owner selected for each index.
- `peryx-ecosystem-*` crates implement core traits and own their protocol code, schemas, settings, routes, tests,
  fixtures, benchmarks, and docs.
- `peryx-ha` defines availability contracts. `peryx-ha-distributed` owns datacenter and multi-datacenter resources and
  workers.
- `peryx-test-support`, `peryx-bench-core`, and `peryx-bench` provide shared test and benchmark infrastructure.
- `peryx` links all shipped implementations and owns process startup and shutdown.

One executable links all shipped ecosystem owners and `peryx-ha-distributed`. Startup configuration activates owners and
selects `none`, `dc`, or `ha`. An unselected owner installs no runtime state or work; `none` skips distributed assembly.

## Tests and CI

Put detailed crate behavior tests under `crates/CRATE/tests/unit/`. A library or binary root may mount a test module
with `#[path]` when the test needs private access. Keep test bodies out of `src/`, and do not widen production
visibility for a test. Put public-boundary tests under `crates/CRATE/tests/integration/` or at the root of
`crates/CRATE/tests/`. System tests belong to the package that owns the executable, external service, or cross-crate
boundary.

`just coverage-native` runs the complete workspace with all features and rejects any uncovered workspace source line.
`just coverage-frontend` applies the same source-line requirement to native and Wasm browser code.

Run focused checks while editing:

```shell
just test
just lint
just coverage-native
just coverage-frontend
just docs
just pre-commit
```

`just all` runs linting, coverage, and documentation. GitHub Actions owns triggers, runners, caches, artifacts, and
matrices; every check runs through a local `just` recipe. Pull requests run the native suite once under coverage instead
of repeating it in crate and system lanes. Nightly CI runs feature powersets, Miri, Loom, sanitizers, mutation, fuzzing,
and the live client boundary without retries or accepted failures.

Docker-backed tests use the local Docker daemon through Testcontainers. This is the same boundary used by GitHub
Actions; there is no separate CI container or CI-only command path.

Cargo-dist generates `.github/workflows/release.yml`. Change `dist-workspace.toml` and run `just release-plan` instead
of editing that workflow.
