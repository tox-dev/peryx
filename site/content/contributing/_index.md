+++
title = "Contributing"
description = "Set up a peryx working tree, the CI gates, the test suites, the docs site, and how to cut a release."
sort_by = "weight"
template = "section.html"
weight = 20
+++

Report bugs, discuss features, and open pull requests at [github.com/tox-dev/peryx](https://github.com/tox-dev/peryx).

## Setting up

Set up a working tree:

```shell
rustup show          # picks the pinned toolchain from rust-toolchain.toml
mise install         # zola, uv, prek, cargo-nextest, cargo-llvm-cov, twine
prek install         # fmt, clippy, and hygiene hooks on every commit
```

[mise](https://mise.jdx.dev) pins the non-Rust tools and removes the need for a system package manager;
[prek](https://github.com/j178/prek) runs the hooks from `.pre-commit-config.yaml`.

## The gates

Run the CI gates before pushing:

```shell
just all
```

`just all` runs linting, pre-commit, every Rust and browser suite, and the 100% line and function coverage gate. Use the
same targets separately while developing, such as `just test`, `just frontend`, or `just coverage-native`.

Run the Linux gate from macOS or Windows without copying the working tree into an image:

```shell
docker compose --profile test run --rm test all
```

Compose bind-mounts the working tree and keeps Cargo, target, npm, and Docker data in named volumes. The nested Docker
daemon supports tests that create containers. Use `docker compose --profile test down` to stop it.

Run the suite with [nextest](https://nexte.st/), not `cargo test`. nextest gives each test its own process; `cargo test`
runs a binary's tests as threads in one process. The web UI tests render Leptos pages, and Leptos drives a per-thread
reactive graph through process-global arenas, so two page renders at once in one process deadlock on a lost wakeup. This
makes `cargo test` flaky, while nextest isolates the renders. The tests also cache the deterministic route table and
serialize their own renders, so a stray `cargo test` no longer hangs; nextest stays the supported runner.

On macOS hosts, nextest starts one test process at a time. Rust creates an output pipe before marking it close-on-exec
on macOS, so concurrent starts can pass one test's descriptor to a sibling and report a false leak. Serial starts close
that race at the cost of a longer local run; Linux and Windows keep nextest's CPU-sized parallelism. Nextest tracks the
underlying race in [nextest#1469](https://github.com/nextest-rs/nextest/issues/1469).

## End-to-end tests

The e2e suite drives real pip, uv, and twine against a spawned peryx binary:

```shell
cargo test -p peryx --features e2e                    # hermetic: local fixture index, no network
cargo test -p peryx --features e2e-live -- e2e_live   # live smoke tests against pypi.org
```

Each test owns an isolated server, fixture, and virtualenv on ephemeral ports, so the suite runs in parallel and
finishes in about two seconds. New index features need a matching e2e test; a client exit code alone does not count as
proof, so assert on peryx's own state or metrics.

## The web UI

`cargo leptos build` compiles the UI's wasm bundle into `ui/pkg/` (mise provides
[cargo-leptos](https://github.com/leptos-rs/cargo-leptos) and node). The [Playwright](https://playwright.dev/) suite
drives the hydrated UI against a real peryx with an uploaded fixture package:

```shell
cargo leptos build
cd tests/frontend
npm ci
npx playwright install chromium
npx playwright test
```

`just coverage-frontend` instruments the native server and the Wasm bundle. It selects a nightly Rust compiler whose
LLVM major matches the stable compiler, records each browser test, and emits separate LCOV reports for native and Wasm
code. `just coverage` merges them with the Rust suites before enforcing complete line and function coverage.

## The documentation site

The [Zola](https://www.getzola.org/) site under `site/` follows the [Diátaxis](https://diataxis.fr/) framework:
tutorials teach, guides solve one task, reference states facts, explanation gives reasons. Put new pages in the quadrant
that matches their job.

```shell
zola --root site serve   # live-reloading preview at 127.0.0.1:1111
```

[Read the Docs](https://readthedocs.org/) builds and hosts the site from `.readthedocs.yaml` on each merge; CI builds it
on each pull request so a broken site blocks the merge.

## Gotchas

### The SSR binary and the wasm bundle must come from one build

`cargo leptos build` writes a matched pair: `target/debug/peryx` (the server that renders HTML) and
`ui/pkg/peryx_web*.wasm` (the bundle that hydrates it). Both embed the same component tree, and hydration only works
when they agree. Mix two builds and the server emits hydration markers the wasm does not expect;
[Leptos](https://leptos.dev/) then panics in the browser (`tachys::hydration::failed_to_cast_marker_node`,
`RuntimeError: unreachable`), leaves `body[data-hydrated]` unset, and causes each Playwright test to time out during
navigation without reporting the cause.

The Playwright harness (`tests/frontend/serve.mjs`) prefers `target/release/peryx` when it exists, and a plain
`cargo build --release` rebuilds only the binary, leaving it paired with a stale debug wasm. After touching UI source,
rerun `cargo leptos build`. If you keep a release binary around, build it with `cargo leptos build --release` so both
halves match, or delete it so the harness falls back to the debug pair.

When a Playwright run fails wholesale at `waitForSelector("body[data-hydrated]")`, open the page in a browser and read
the console. A hydration panic there points at a mismatched build pair, so rebuild before you suspect the test.

### Off-by-default features need their own unit tests

A subsystem disabled by default is an `Option<T>` that stays `None`, which removes it from the request path (see the
zero-overhead contract in the architecture docs). Integration tests that drive the default server cannot reach its code.
For example, disabling the rate limiter makes peryx omit the enforce layer, so only a direct unit test runs a driver
method such as `classify_route`. Write that unit test before the 100% coverage gate reports the uncovered line.

## Conventions

- Commits: imperative subject up to 50 characters, no period; a wrapped body explaining what and why for anything
  non-obvious. Keep commits atomic.
- Markdown wraps at 120 columns via `mdformat` (the pre-commit hook handles it).
- Code style is whatever `cargo fmt` and the [clippy](https://github.com/rust-lang/rust-clippy) configuration in
  `Cargo.toml` say; fix findings rather than suppressing them, and give any unavoidable suppression a reason.
