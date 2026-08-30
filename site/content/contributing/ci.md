+++
title = "Continuous integration"
description = "Run the same validation on a workstation and in GitHub Actions."
weight = 20
+++

GitHub Actions owns triggers, permissions, runners, caches, artifacts, matrices, and job dependencies. The `justfile`
owns validation commands so each CI command also runs from a checkout.

## Pull requests

The required workflow runs these job groups:

- `source`: Rust formatting, `cargo check`, Clippy, and dependency policy
- `automation`: repository hooks and workflow validation
- `contracts`: snapshots, the release plan, and Cargo discovery of publishable packages
- `semver`: public API compatibility for each publishable package
- `platform`: platform-boundary tests on macOS and Windows
- `coverage`: the native workspace suite with all features
- `frontend`: native and Wasm browser coverage
- `docs`: rustdoc, Markdown, Mermaid regeneration, and the site build

The coverage jobs reject uncovered source lines. `ci-gate` gives branch protection one check name and fails unless every
required job succeeds.

CodSpeed runs the ecosystem benchmark packages on standard GitHub-hosted runners in
[simulation mode][codspeed-simulation]. This avoids quota-limited Macro Runners. Run the same benchmark path with
`just codspeed PACKAGE`.

## Test synchronization

Tests wait for observable state changes. Child-process cases use `ProcessHarness::spawn_until_event`,
`Node::await_event`, or the topology event stream. In-process async cases use channels or
[`tokio::sync::Notify`][tokio-notify]. Code that measures elapsed time uses [Tokio's paused clock][tokio-testing].
Deadlines bound failed waits, and the CI profile supplies a [per-test termination guard][nextest-timeouts].

## Nightly analysis

The nightly workflow runs feature combinations, direct dependency lower bounds, Miri, Loom, AddressSanitizer, mutation
testing, each cargo-fuzz target, and the live PyPI client boundary. Each matrix leg invokes a public Just recipe.

Sanitizer and mutation jobs build [Nextest archives][nextest-archives] once, then run partitions from those archives.
AddressSanitizer follows Rust's [`-Zsanitizer` and `-Zbuild-std` invocation][rust-sanitizers]. Nextest 0.9.143
[classifies Rust's `gnuasan` target as a custom target][nextest-sanitizer-target], but Rust does not publish custom
target JSON for that built-in target. The workflow uses the standard Linux target.

The async suite does not run under ThreadSanitizer. [Tokio issue 7299][tokio-tsan] records internal false positives and
identifies Miri and Loom as its race-analysis tools; nightly CI runs both.

## Local commands

Install the locked tools, then run the recipe named by a CI job:

```console
mise install --locked
just lint
just platform-test
just coverage-native
just frontend-deps
just coverage-frontend
just docs
```

Browser recipes install their checksum-verified Chrome for Testing revision from the scoped `browser` mise environment.
Chrome for Testing ships no Linux ARM or Windows ARM builds, so `mise.browser.lock` covers the four platforms it does
publish.

`just test` is hermetic. `just storage-s3` and `just coverage-native` require a running Docker daemon for the MinIO
boundary tests.

Nightly commands are local too:

```console
just features
just direct-minimum
just miri
just loom
just sanitizer-address
just mutation-baseline
just mutation 1/8
just fuzz peryx-ecosystem-oci oci_reference 60
just e2e-live
```

Generated files stay under `.tox/`. `just coverage-clean`, `just clean`, and `just clean-all` remove increasing amounts
of local build state.

[codspeed-simulation]: https://github.com/CodSpeedHQ/codspeed/blob/v5.2.1/README.md
[nextest-archives]: https://nexte.st/docs/ci-features/archiving/
[nextest-sanitizer-target]: https://github.com/nextest-rs/nextest/blob/cargo-nextest-0.9.143/nextest-runner/src/cargo_config/target_triple.rs
[nextest-timeouts]: https://nexte.st/docs/features/slow-tests/#terminating-tests-after-a-timeout
[rust-sanitizers]: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html
[tokio-notify]: https://docs.rs/tokio/latest/tokio/sync/struct.Notify.html
[tokio-testing]: https://tokio.rs/tokio/topics/testing#pausing-and-resuming-time-in-tests
[tokio-tsan]: https://github.com/tokio-rs/tokio/issues/7299
