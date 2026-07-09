+++
title = "Contributing"
description = "Set up a peryx working tree, the CI gates, the test suites, the docs site, and how to cut a release."
sort_by = "weight"
template = "section.html"
weight = 20
+++

peryx lives at [github.com/tox-dev/peryx](https://github.com/tox-dev/peryx). Bug reports, feature discussions, and pull
requests are welcome there.

## Setting up

Two installs bootstrap a working tree:

```shell
rustup show          # picks the pinned toolchain from rust-toolchain.toml
mise install         # zola, uv, prek, cargo-nextest, cargo-llvm-cov, twine
prek install         # fmt, clippy, and hygiene hooks on every commit
```

[mise](https://mise.jdx.dev) pins the non-Rust tools, so nothing needs a system package manager;
[prek](https://github.com/j178/prek) runs the hooks from `.pre-commit-config.yaml`.

## The gates

CI holds each pull request to the same bar; run the gates locally before pushing:

```shell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo llvm-cov nextest --workspace --ignore-filename-regex 'main\.rs' \
  --fail-under-lines 100 --fail-under-functions 100
```

Line and function coverage stay at 100%. CI reports region coverage without gating it, because no test on stable Rust
can reach compiler-generated branches (async expansions, drop glue).

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

`cargo leptos build` compiles the UI's wasm bundle into `ui/pkg/` (mise provides cargo-leptos and node). The Playwright
suite drives the hydrated UI against a real peryx with an uploaded fixture package:

```shell
cargo build -p peryx
cargo leptos build
cd tests/frontend
npm ci
npx playwright install chromium
npx playwright test
```

The UI crate sits outside the `llvm-cov` gate: wasm cannot be coverage-instrumented and event handlers only run in a
browser, so the Playwright suite and peryx's server-side render tests are its gates instead.

## The documentation site

The site you are reading is [Zola](https://www.getzola.org/) under `site/`, structured by the
[Diátaxis](https://diataxis.fr/) framework: tutorials teach, guides solve one task, reference states facts, explanation
gives reasons. Put new pages in the quadrant that matches their job.

```shell
zola --root site serve   # live-reloading preview at 127.0.0.1:1111
```

Read the Docs builds and hosts the site from `.readthedocs.yaml` on each merge; CI builds it on each pull request so a
broken site blocks the merge.

## Gotchas

Two dev-environment behaviors are non-obvious enough to have cost real debugging time.

### The SSR binary and the wasm bundle must come from one build

`cargo leptos build` writes a matched pair: `target/debug/peryx` (the server that renders HTML) and
`ui/pkg/peryx_web*.wasm` (the bundle that hydrates it). Both embed the same component tree, and hydration only works
when they agree. Mix two builds and the server emits hydration markers the wasm does not expect; Leptos then panics in
the browser (`tachys::hydration::failed_to_cast_marker_node`, `RuntimeError: unreachable`), never sets
`body[data-hydrated]`, and every Playwright test times out at navigation with no hint as to why.

The Playwright harness (`tests/frontend/serve.mjs`) prefers `target/release/peryx` when it exists, and a plain
`cargo build --release` rebuilds only the binary, leaving it paired with a stale debug wasm. After touching UI source,
rerun `cargo leptos build`. If you keep a release binary around, build it with `cargo leptos build --release` so both
halves match, or delete it so the harness falls back to the debug pair.

When a Playwright run fails wholesale at `waitForSelector("body[data-hydrated]")`, open the page in a browser and read
the console. A hydration panic there points at a mismatched build pair, so rebuild before you suspect the test.

### Off-by-default features need their own unit tests

A subsystem that is disabled by default is an `Option<T>` that stays `None`, so it is absent from the request path
rather than skipped on it (see the zero-overhead contract in the architecture docs). Integration tests that drive the
default server therefore never reach its code. The rate limiter is the standard example: with it off, peryx omits the
enforce layer entirely, so a driver method like `classify_route` runs only under a direct unit test. The 100% coverage
gate will catch the omission, but it is faster to write the unit test up front than to chase the uncovered line.

## Conventions

- Commits: imperative subject up to 50 characters, no period; a wrapped body explaining what and why for anything
  non-obvious. Keep commits atomic.
- Markdown wraps at 120 columns via `mdformat` (the pre-commit hook handles it).
- Code style is whatever `cargo fmt` and the clippy configuration in `Cargo.toml` say; fix findings rather than
  suppressing them, and give any unavoidable suppression a reason.
