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
- `platform`: platform-boundary tests on macOS and Windows
- `coverage`: the native workspace suite with all features
- `frontend`: native and Wasm browser coverage
- `docs`: rustdoc, Markdown, Mermaid regeneration, and the site build

There is no `semver` job. Every crate reads `0.0.1`, a version at which Cargo permits any change, so cargo-semver-checks
compared each package against itself, assumed a major bump and skipped all 254 checks while reporting success. Seven
shards spent 43 minutes of runner time per run to check nothing, and a reader could not tell that green from a green
that had verified something. Nothing consumes these crates as libraries either: a release ships binaries through `dist`
and a PyPI package, and no workflow runs `cargo publish`.

Bring the job back when a crate is published, or when the workspace reaches a version where a bump means something. It
needs the `baseline` output on the `contracts` job as well, which went with it. Until then `just semver` answers the
same question on demand and states a release type so the checks run.

A `LEAK` or `LEAK-FAIL` from nextest requires investigation. Nextest 0.9.145 fixes the macOS capture-pipe inheritance
race in [nextest-rs/nextest#3553](https://github.com/nextest-rs/nextest/pull/3553), so an observed leak is no longer
explained by that upstream defect. Do not increase `leak-timeout`, exclude a test, retry, or serialise the run instead
of finding the process that retains the descriptor.

The nightly mutation run examines production code only. `.cargo/mutants.toml` excludes benchmark workloads under
`crates/*/src/bench/`, the shared harness in `crates/peryx-test-support/`, and the fixture binaries under
`crates/*/tests/`. A surviving mutant means the code could behave another way and no test would notice, which marks a
gap where the behaviour is a promise to somebody. None of those three paths promises anything outside this repository,
so a survivor in them is noise that each run reports again. The exclusions take 18,902 mutants down to 17,839.

The harness is the one worth arguing about, since a fault injector that stopped injecting would leave every test using
it green while testing nothing. That failure shows up red instead. A test that arms a fault asserts the faulted outcome,
so it fails when the fault does not arrive, and `-D dead_code` covers harness behaviour no test reaches. Both checks
land before mutation asks the question, and the inventory agrees, with no mutant in that crate surviving a run. Add a
path back if that stops holding, and say in the config why.

`cargo mutants` does not evaluate `cfg`, so it also proposes mutants in code the nightly's native, all-features Linux
build compiles out: the browser halves of the web loaders, which need `gloo_net` or `EventSource`, the wasm coverage
export, and the macOS `sysctl` call. No test in that build can catch those, so `exclude_re` in `.cargo/mutants.toml`
names them by function. It names functions and never files, because the same files hold logic the suite does pin, and a
whole-file exclusion discards that with it: excluding the sixteen files these functions live in would have hidden 155
survivors and thrown away 228 mutants the suite kills.

A function goes on that list only when it cannot run natively. Logic that merely sits behind a browser or platform gate
does not qualify. Widen its gate to `any(test, ...)` so the native suite compiles it, then pin it with a test, as the
stats and status document conversions, the URL builders, the page query and paging handlers and `sysctl_with` are. The
list also holds one mutant that is the original under another spelling: `Poll::from(None)` in `Ended::poll_frame`, where
`From<T> for Poll<T>` is `Poll::Ready`. An entry of that kind names the single replacement and states the proof beside
it, and only when the code has no simpler spelling that removes the mutant. After changing the list, diff
`cargo mutants --list` against `cargo mutants --list --no-config` and check that every line it removes is one you meant.

Excluding paths shortens the matrix rather than the shards. The nightly derives its shard count from the same list it
mutates, at `mutation-shard-count "$(just mutation-count)" 128`, so the run goes from 148 shards to 140 with each still
targeting 128 mutants. A shard takes as long as it did.

Most shards end with `The hosted runner lost communication with the server`, and a runner that disappears uploads no
artifact and leaves no log, so `just mutation-observed` samples the shard every 60 seconds into
`.tox/mutants/resource.log`, which rides the artifact upload. Each line carries how many mutants finished and which one
finished last, the cgroup's memory and pid counters, the kernel's pressure files, the memory the machine has left, and
free space on the filesystem holding the workspace. GitHub publishes this runner class as 4 vCPU, 16 GB RAM and 14 GB
SSD, and a shard holds a restored Cargo cache, a full `--all-features` debug target tree, and 128 successive suite runs
writing into `.tox/tmp`, so a trace from a shard that survives is what says which of the three runs out.
`_mutation-telemetry-contract` fails if a sample stops reporting any of them.

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

`just test` is hermetic. `just storage-s3` and `just coverage-native` require a running Docker daemon for the Versity S3
Gateway boundary tests.

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
[nextest-sanitizer-target]: https://github.com/nextest-rs/nextest/blob/cargo-nextest-0.9.145/nextest-runner/src/cargo_config/target_triple.rs
[nextest-timeouts]: https://nexte.st/docs/features/slow-tests/#terminating-tests-after-a-timeout
[rust-sanitizers]: https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html
[tokio-notify]: https://docs.rs/tokio/latest/tokio/sync/struct.Notify.html
[tokio-testing]: https://tokio.rs/tokio/topics/testing#pausing-and-resuming-time-in-tests
[tokio-tsan]: https://github.com/tokio-rs/tokio/issues/7299
