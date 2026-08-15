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

`just crate-contract PACKAGE OUTPUT` builds and runs the package's targets, checks its source inventory, and requires
100% line and function coverage, including test source. Lint, package, API, feature, and documentation checks run in
separate lanes.

Run focused checks while editing:

```shell
just test
just crate-contract peryx-core .tox/crate-contracts/peryx-core
just lint
just coverage
just docs
just pre-commit
```

`just all` runs linting, complete Linux coverage, documentation, and CI-safe hooks. Run `crate-contract` for each
changed package. Use `just system-test` when a change reaches a process or external-service boundary.

GitHub Actions splits pull-request work into lint, platform, weighted crate-contract, system, frontend, owner fuzz,
mutation, documentation, and coverage lanes. The gate fails when a required result is missing or unsuccessful. Workflow
files own triggers, runners, caches, artifacts, and dependencies. `just` recipes and repository scripts own validation,
so each platform-compatible CI target runs from a checkout.

Pull requests mutate all changed production candidates up to 32. Larger sets run a deterministic round-robin sample
split across eight jobs. Each job prints the candidate count and selected shard. Scheduled CI mutates the full workspace
across eight shards.

`compose.yaml` bind-mounts the checkout. `just linux COMMAND` runs a recipe in the 8 GiB Linux test service, while
`just linux-system COMMAND` adds Docker-backed services. Use a 16 GiB wrapper only after the 8 GiB run reports memory
pressure.

Cargo-dist generates `.github/workflows/release.yml`. Change `dist-workspace.toml` and run `just release-plan` instead
of editing that workflow.
