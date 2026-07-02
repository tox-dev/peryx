+++
title = "Architecture"
description = "One process, two stores, and a read-through cache in front of upstream indexes."
weight = 1
+++

velox is a single async Rust process built on [axum](https://github.com/tokio-rs/axum) and [tokio](https://tokio.rs/). A request travels through three layers:

1. **Routing.** The HTTP layer resolves the path against the configured index routes by longest prefix, so routes
   are configuration data rather than compiled-in paths.
2. **Resolution.** The cache layer answers from local state when it can and talks to upstreams when it must. An
   overlay walks its layers in order and merges their answers.
3. **Storage.** Two stores under one data directory hold all state.

## The metadata store

Project pages, file-to-URL mappings, uploads, and the change serial live in [redb](https://www.redb.org/), an
embedded, crash-safe, copy-on-write B-tree in pure Rust. redb gives one writer and many concurrent readers with
snapshot isolation, which fits an index server's read-heavy traffic without an external database.

Cached upstream pages carry their `ETag` and fetch time. Within `cache_ttl_secs` velox serves the cached page as is;
after that it revalidates with `If-None-Match`, and a `304` refreshes the clock without a body transfer. A `5xx` or
an unreachable upstream falls back to the cached copy, so a pypi.org outage degrades to stale-but-working.

## The blob store

Artifacts live in a content-addressed store keyed by sha256, fanned out two hex levels deep
(`sha256/ab/cd/<digest>`). Writes go to a temp file, fsync, then an atomic rename, so a crash cannot leave a partial
blob visible; the path is the digest, so anything present is by construction correct. One wheel uploaded to two
indexes, or cached from two mirrors, occupies disk once.

Downloads verify: velox hashes fetched bytes against the digest the index advertised before storing or serving
them, and uploads verify the digest the client declared.

## Why PEP 658 matters here

Resolvers spend most of their network time learning dependencies. The [PEP 658/714](https://peps.python.org/pep-0658/) `.metadata` sibling lets pip and
uv fetch a few kilobytes of core metadata instead of a multi-megabyte wheel per candidate. velox advertises the
sibling, fetches it from the upstream on first use, verifies it against the digest from the index page, and caches
it like any blob. The `velox_metadata_requests_total` metric counts these; the end-to-end tests assert on it to
prove real clients take this path.

## The web UI

The UI is a [Leptos](https://leptos.dev/) application compiled twice from one codebase: natively into the server,
which renders every page to HTML, and to WebAssembly (by [cargo-leptos](https://github.com/leptos-rs/cargo-leptos)),
which hydrates the page in the browser for reactivity: live counters, filter-as-you-type, and the upload-management
buttons. Pages work without the bundle, so the server never depends on a wasm toolchain.

This split also decides how the UI is tested. The server half is ordinary Rust: velox's test suite renders each page
through the real router and asserts on the HTML. The browser half cannot feed the coverage gate, because
`wasm32-unknown-unknown` has no coverage instrumentation and event handlers only execute in a browser; a Playwright
suite drives the hydrated UI instead (search, package pages, the archive browser, and token-authenticated yank and
delete), which is the stronger check for interactive behavior anyway.

The UI reads velox's own public API: `/+status` for the dashboard, the PEP 691 simple endpoints for package data, and
the `inspect` endpoints for archive contents. Anything the UI shows, a script can fetch the same way.
