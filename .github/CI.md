# Continuous integration

GitHub Actions owns triggers, permissions, runners, caches, artifacts, matrices, and job dependencies. Repository checks
live in the `justfile`, so the same commands run locally and in CI.

## Pull requests

The required workflow runs eight independent job groups:

- `source`: formatting, `cargo check`, Clippy, and dependency policy
- `automation`: repository hooks and workflow validation
- `contracts`: snapshots, the release plan, and Cargo discovery of publishable packages
- `semver`: public API compatibility for each publishable package, parallelized by GitHub's matrix
- `platform`: platform-boundary tests on macOS and Windows
- `coverage`: the complete native workspace suite with all features
- `frontend`: native and Wasm browser coverage
- `docs`: rustdoc, Markdown, and the assembled site

`coverage` replaces a separate Linux test job. It runs the tests once under `cargo llvm-cov` and rejects any uncovered
source line. The frontend job follows wasm-bindgen's LLVM coverage procedure and applies the same line requirement to
its merged native and Wasm report.

The `ci-gate` job gives branch protection one stable check name. It only evaluates GitHub job results; test policy
remains in `just` recipes and standard tool configuration.

CodSpeed runs both ecosystem benchmark packages in parallel. Each leg builds through `just codspeed-build` and uses
CodSpeed's official action to run and upload the result. `just codspeed` provides the equivalent local command on a
supported Linux host.

## Nightly analysis

The nightly workflow runs work that is too expensive or specialized for every pull request:

- every feature combination
- direct dependency lower bounds
- Miri and Loom
- AddressSanitizer and ThreadSanitizer
- eight cargo-mutants shards
- each cargo-fuzz target
- the live PyPI client boundary

Each matrix leg invokes a public `just` recipe. Nextest runs each test once, and each command propagates its exit
status.

## Local commands

Install the versions declared in `mise.toml`, then run the same entry points used by GitHub Actions:

```console
mise install
just lint
just platform-test
just coverage-native
just frontend-deps
just frontend-browser-deps
just coverage-frontend
just docs
```

`just test` is hermetic. `just storage-s3` and `just coverage-native` require a running Docker daemon for the MinIO
boundary tests and fail before compilation when Docker is unavailable.

Nightly commands are also local:

```console
just features
just direct-minimum
just miri
just loom
just sanitizer-address
just sanitizer-thread
just mutation 1/8
just fuzz peryx-ecosystem-oci oci_reference 60
just e2e-live
```

Generated files stay under `.tox/`. `just coverage-clean`, `just clean`, and `just clean-all` remove progressively more
local state.
