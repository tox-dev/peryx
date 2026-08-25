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

CodSpeed runs both ecosystem benchmark packages in parallel on its stable bare-metal runners. Each leg builds through
`just codspeed-build` and uses CodSpeed's official action in walltime mode. `just codspeed` provides the equivalent
local command and accepts the measurement mode as its second argument.

## Nightly analysis

The nightly workflow runs work that is too expensive or specialized for every pull request:

- every feature combination
- direct dependency lower bounds
- Miri and Loom
- AddressSanitizer and ThreadSanitizer, each built once and split evenly with Nextest
- one mutation baseline with the same workspace, features, runner, and test filter as cargo-mutants, followed by shards
  of at most 256 mutants
- each cargo-fuzz target
- the live PyPI client boundary

Each matrix leg invokes a public `just` recipe. Nextest runs each test once, and each command propagates its exit
status. Sanitizer builds use [Nextest archives][nextest-archives], so partition jobs transfer the instrumented test
binaries instead of rebuilding them. We build on the standard Linux target with
[Rust's documented `-Zsanitizer` and `-Zbuild-std` invocation][rust-sanitizers]. During archive restore, Nextest 0.9.143
[classifies Rust's `gnuasan` and `gnutsan` targets as custom targets][nextest-sanitizer-target] and requires custom
target JSON. Rust does not provide custom JSON for these built-in targets.

ThreadSanitizer uses its [documented global I/O synchronization model][tsan-io] because Tokio registers sources and
consumes epoll events through different descriptors. Reports still halt the affected test; no test or function is
suppressed.

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
just sanitizer-archive address .tox/address.tar.zst
just sanitizer-run .tox/address.tar.zst slice:1/8
just mutation 1/8
just fuzz peryx-ecosystem-oci oci_reference 60
just e2e-live
```

Generated files stay under `.tox/`. `just coverage-clean`, `just clean`, and `just clean-all` remove progressively more
local state.

[nextest-archives]: https://nexte.st/docs/ci-features/archiving/
[nextest-sanitizer-target]: https://github.com/nextest-rs/nextest/blob/cargo-nextest-0.9.143/nextest-runner/src/cargo_config/target_triple.rs#L32-L44
[rust-sanitizers]: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html
[tsan-io]: https://github.com/google/sanitizers/wiki/ThreadSanitizerFlags#runtime-flags
