+++
title = "Architecture"
description = "One process, two stores, and a read-through cache in front of upstream indexes."
weight = 1
+++

velodex is a single async Rust process built on [axum](https://github.com/tokio-rs/axum) and [tokio](https://tokio.rs/). A request travels through three layers:

1. **Routing.** The HTTP layer resolves the path against the configured index routes by longest prefix, so routes
   are configuration data rather than compiled-in paths.
2. **Resolution.** The cache layer answers from local state when it can and talks to upstreams when it must. An
   overlay walks its layers in order and merges their answers.
3. **Storage.** Two stores under one data directory hold all state.

## The metadata store

Project pages, file-to-URL mappings, uploads, and the change serial live in [redb](https://www.redb.org/), an
embedded, crash-safe, copy-on-write B-tree in pure Rust. redb gives one writer and many concurrent readers with
snapshot isolation, which fits an index server's read-heavy traffic without an external database.

Cached upstream pages carry their `ETag`, fetch time, and the freshness lifetime the upstream granted via
`Cache-Control` (`s-maxage` over `max-age`; `cache_ttl_secs` fills in when the server granted none). Within that
window velodex serves the cached page as is; after it, the next request revalidates with `If-None-Match`, and a `304`
refreshes the clock without a body transfer. A `5xx` or an unreachable upstream falls back to the cached copy, so a
pypi.org outage degrades to stale-but-working.

A background sweep revalidates every stale cached page once a minute, so an upstream change lands within about one
freshness window even when nobody requests the page. Each detected change is logged and counted (`changed` in
[`/+stats`](@/reference/endpoints.md)); ETag hits keep the sweep cheap, since an unchanged page answers `304` with
no body.

## The blob store

Artifacts live in a content-addressed store keyed by sha256, fanned out two hex levels deep
(`sha256/ab/cd/<digest>`). Writes go to a temp file, fsync, then an atomic rename, so a crash cannot leave a partial
blob visible; the path is the digest, so anything present is by construction correct. One wheel uploaded to two
indexes, or cached from two mirrors, occupies disk once.

Downloads verify: velodex hashes fetched bytes against the digest the index advertised before storing or serving
them, and uploads verify the digest the client declared.

## The serving pipeline

A cold page or file streams: bytes flow from the upstream to the client as they arrive, while a tee writes them
aside for the cache. For pages, a chunk-at-a-time transformer rewrites each `files[]` element mid-flight (URL
rewriting, local-file injection, yank and hide overrides), so the client starts parsing before the upstream download
finishes. For files, the tee hashes into a temp file that is verified and renamed into the blob store after the
client already has its bytes; a hash mismatch forwards (pip and uv verify digests themselves) but is never cached,
and is counted as `rejected`.

Concurrent misses for the same page or file share one upstream fetch (a per-key single-flight lock), warm pages
transform in memory from the raw cached body, and hot pages come from an in-memory cache keyed by a mutation epoch,
so uploads and overrides invalidate instantly. File downloads use HTTP/1.1 upstream: HTTP/2 would multiplex
every artifact over one TCP connection and its single congestion window.

## Usage metrics

Handlers record events (page served, file downloaded, upload accepted, refresh outcome) with one non-blocking
channel send; a dedicated thread aggregates them into an index → project → file tree. The request path never takes
the aggregation lock; recording costs one channel send. The tree serves
[`/+stats`](@/reference/endpoints.md), the dashboard's usage drill-down, and the per-index Prometheus counters.

## Why PEP 658 matters here

Resolvers spend most of their network time learning dependencies. The [PEP 658/714](https://peps.python.org/pep-0658/) `.metadata` sibling lets pip and
uv fetch a few kilobytes of core metadata instead of a multi-megabyte wheel per candidate. velodex advertises the
sibling, fetches it from the upstream on first use, verifies it against the digest from the index page, and caches
it like any blob. The `velodex_metadata_requests_total` metric counts these; the end-to-end tests assert on it to
prove real clients take this path.

## The web UI

The UI is a [Leptos](https://leptos.dev/) application compiled twice from one codebase: natively into the server,
which renders every page to HTML, and to WebAssembly (by [cargo-leptos](https://github.com/leptos-rs/cargo-leptos)),
which hydrates the page in the browser for reactivity: live counters, filter-as-you-type, and the upload-management
buttons. Pages work without the bundle, so the server never depends on a wasm toolchain.

This split also decides how the UI is tested. The server half is ordinary Rust: velodex's test suite renders each page
through the real router and asserts on the HTML. The browser half cannot feed the coverage gate, because
`wasm32-unknown-unknown` has no coverage instrumentation and event handlers only execute in a browser; a Playwright
suite drives the hydrated UI instead (search, package pages, the archive browser, and token-authenticated yank and
delete), which is the stronger check for interactive behavior anyway.

The UI reads velodex's own public API: `/+status` for the dashboard, the PEP 691 simple endpoints for package data, and
the `inspect` endpoints for archive contents. Anything the UI shows, a script can fetch the same way.
