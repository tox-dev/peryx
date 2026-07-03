+++
title = "Architecture"
description = "How one process serves a pip install: the request path, the streaming cache, freshness, and the two stores."
weight = 1
+++

Start with the concrete case: a CI job runs `uv pip install pandas` against a velodex that has never seen pandas.
uv asks for the pandas index page, then for a handful of `.metadata` files, then for the wheels it chose. velodex
has none of it. The naive proxy would download each thing, store it, and then serve it, adding a full download of
buffering delay on top of each upstream fetch. Most of this page explains how velodex avoids that: the client
receives bytes at upstream wire speed on a cold cache, and from local disk and memory afterwards.

velodex is a single async Rust process built on [axum](https://github.com/tokio-rs/axum) and
[tokio](https://tokio.rs/). A request travels through three layers:

1. **Routing.** The HTTP layer resolves the path against the configured index routes by longest prefix, so routes
   are configuration data rather than compiled-in paths.
1. **Resolution.** The cache layer answers from local state when it can and talks to upstreams when it must. An
   overlay walks its layers in order and merges their answers.
1. **Storage.** Two stores under one data directory hold all state.

{% mermaid() %}
flowchart LR
client["pip / uv / twine"] --> router["route resolver"]
subgraph velodex["velodex, one process"]
router --> cache["cache layer"]
cache --> hot["hot page cache (RAM)"]
cache --> meta["metadata store (redb)"]
cache --> blobs["artifact store (disk)"]
end
cache -.->|"only on miss"| upstream["pypi.org, or any mirror"]
classDef accent fill:#0072B2,stroke:#0072B2,color:#ffffff
class cache accent
{% end %}

## How a page is served

A simple-index page (say `/root/pypi/simple/pandas/`) can be answered three ways, tried in order of cost:

1. **Hot:** the transformed page sits in an in-memory cache, keyed by a mutation epoch so any upload or override
   invalidates it instantly. Serving is a lookup and a memcpy.
1. **Warm:** the raw upstream page sits in the metadata store and is still within its freshness window. velodex
   transforms it for the requesting route in one in-memory pass (file URLs rewritten, local uploads injected,
   yanked and hidden files applied) and remembers the result in the hot cache.
1. **Cold:** nothing usable is stored. velodex opens the upstream request and streams.

{% mermaid() %}
flowchart LR
req["GET simple/pandas/"] --> hot{"hot cache?"}
hot -->|hit| serve["serve from RAM"]
hot -->|miss| warm{"raw page fresh?"}
warm -->|yes| transform["transform in memory"] --> serve
warm -->|no| cold["stream from upstream"] --> serve2["serve while caching"]
classDef good fill:#009E73,stroke:#009E73,color:#ffffff
classDef accent fill:#0072B2,stroke:#0072B2,color:#ffffff
class serve good
class cold,serve2 accent
{% end %}

The stored form is always the **raw upstream document** (HTML upstreams are canonicalized to PEP 691 JSON once, at
fetch time). Transformation happens per request. That ordering matters for overlays: one cached pypi.org page can
serve any number of routes that layer it, each with different local files shadowing it, without storing a variant
per route.

## How bytes reach the client before they reach the disk

The cold path is where proxies lose their users. velodex never buffers a whole response to work on it; both pages and
artifacts stream, with the caching work riding along:

{% mermaid() %}
sequenceDiagram
participant C as uv
participant V as velodex
participant U as upstream
C->>+V: GET simple/pandas/
V->>+U: GET (If-None-Match)
U-->>-V: 200, JSON streams
V-->>-C: transformed JSON, chunk by chunk
Note over V: raw page persists before the<br/>final chunk, so file lookups<br/>that follow always resolve
C->>+V: GET files/{sha256}/pandas…whl
V->>+U: GET wheel
U-->>-V: bytes
V-->>-C: the same bytes, teed to a temp file
Note over V: after the client has everything:<br/>verify sha256, rename into the store
{% end %}

For pages, a chunk-at-a-time transformer rewrites each `files[]` element mid-flight (URL rewriting, local-file
injection, yank and hide overrides), so the client starts parsing while the upstream transfer is still running.
For artifacts, the tee hashes into a temp file that is verified and atomically renamed into the store after the
client already has its bytes. A digest mismatch still forwards (pip and uv verify hashes themselves) but is never
cached, and shows up as `rejected` in the [usage counters](@/reference/endpoints.md).

File URLs put the sha256 in the path because it is the real storage key. The filename is kept for installer behavior,
browser save names, and operator logs, but velodex treats it as one percent-encoded path segment and rejects decoded
separators, traversal, and control characters. Archive inspection uses the same rule for the distribution filename
and passes member paths in a query parameter so member names can contain `/` without becoming route structure. The
inspector opens cached blobs from disk and returns member text by byte offset, so looking at a large generated file does
not require loading the whole archive member into server memory or the browser.

Three more decisions keep the cold path at wire speed:

- **Single-flight.** Resolvers fire many requests for the same project concurrently. Concurrent misses for one
  page or file share one upstream fetch; the rest wait for the first and serve from its result.
- **Nothing durable blocks the response.** Page records commit to redb without an fsync (losing a cache entry in a
  crash costs a refetch, nothing more), and artifact verification runs after the client's last byte.
- **HTTP/1.1 for artifact downloads.** HTTP/2 would multiplex every concurrent wheel over one TCP connection and
  its single congestion window; one connection per artifact keeps large parallel downloads at full bandwidth.

## When does cached content expire?

Artifacts never do. They are addressed by sha256, so "a new version of the file" is by definition a different
file with a different address; anything in the store is correct forever.

Pages do. Each cached page carries the freshness lifetime its upstream granted via `Cache-Control` (`s-maxage`
over `max-age`; pypi.org grants 600 seconds). When the server grants none (the header is absent, `no-cache`,
`no-store`, or zero), the configured `cache_ttl_secs` fallback of 300 seconds applies.

{% mermaid() %}
stateDiagram-v2
[\*] --> Fresh: first fetch
Fresh --> Stale: lifetime lapses
Stale --> Fresh: 304, nothing changed
Stale --> Fresh: 200, new content
Stale --> ServedStale: upstream down
ServedStale --> Fresh: upstream back
{% end %}

A stale page is not dropped: the next request revalidates it with `If-None-Match`, and the common answer is a
`304` with no body, which just resets the clock. A background sweep revalidates every stale page once a minute, so
an upstream change lands within about one freshness window even when nobody requests the page; each detected
change is logged and counted. When the upstream errors or is unreachable, the stale copy serves, and a pypi.org
outage degrades to stale-but-working rather than red builds.

## The metadata store

Project pages, file-to-URL mappings, uploads, and the change serial live in [redb](https://www.redb.org/), an
embedded, crash-safe, copy-on-write B-tree in pure Rust. redb gives one writer and many concurrent readers with
snapshot isolation, which fits an index server's read-heavy traffic without an external database. Page records use
a framed encoding (a small JSON header line, then the raw body bytes), so a multi-megabyte page is not re-encoded
as JSON numbers and header-only scans (the freshness sweep) skip the body.

## The artifact store

Artifacts live in a content-addressed store keyed by sha256, fanned out two hex levels deep
(`sha256/ab/cd/<digest>`). Writes go to a temp file, fsync, then an atomic rename, so a crash cannot leave a
partial blob visible; the path is the digest, so anything present is by construction correct. One wheel uploaded
to two indexes, or cached from two mirrors, occupies disk once.

## Why PEP 658 matters here

Resolvers spend most of their network time learning dependencies. The
[PEP 658/714](https://peps.python.org/pep-0658/) `.metadata` sibling lets pip and uv fetch a few kilobytes of core
metadata instead of a multi-megabyte wheel per candidate. velodex advertises the sibling, fetches it from the
upstream on first use, verifies it against the digest from the index page, and caches it like any blob. The
`velodex_metadata_requests_total` metric counts these; the end-to-end tests assert on it to prove real clients
take this path. Few third-party indexes serve PEP 658 yet, so fronting one with velodex can make resolution
faster than the upstream itself once metadata is cached.

## Usage metrics

Handlers record events (page served, file downloaded, upload accepted, refresh outcome) with one non-blocking
channel send; a dedicated thread aggregates them into an index → project → file tree. The request path never takes
the aggregation lock; recording costs one channel send. The tree serves
[`/+stats`](@/reference/endpoints.md), the dashboard's usage drill-down, and the per-index Prometheus counters.

## Distribution

velodex ships one static binary through two channels. GitHub releases carry per-platform archives and installer
scripts (built by [dist](https://axodotdev.github.io/cargo-dist/)); these copies carry the `self-update` feature
and an install receipt, so `velodex self update` can replace them in place. PyPI carries the same binary wrapped
in a `bindings = "bin"` wheel: Python-shop operators get velodex through the tooling they already run (`uv tool install`, a `requirements.txt` line, an internal mirror) without a second artifact channel, and since no Python
ABI is involved, one wheel per platform serves every interpreter. Wheel installs have no self-update: pip owns
that file, and the updater refuses copies without a receipt rather than fight it.

## The web UI

The UI is a [Leptos](https://leptos.dev/) application compiled twice from one codebase: natively into the server,
which renders every page to HTML, and to WebAssembly (by [cargo-leptos](https://github.com/leptos-rs/cargo-leptos)),
which hydrates the page in the browser for reactivity: live counters, filter-as-you-type, and the upload-management
buttons. Pages work without the bundle, so the server never depends on a wasm toolchain.

This split also decides how the UI is tested. The server half is ordinary Rust: velodex's test suite renders each
page through the real router and asserts on the HTML. The browser half cannot feed the coverage gate, because
`wasm32-unknown-unknown` has no coverage instrumentation and event handlers only execute in a browser; a Playwright
suite drives the hydrated UI instead (search, package pages, the archive browser, and token-authenticated yank and
delete), which is the stronger check for interactive behavior anyway.

The UI reads velodex's own public API: `/+status` for the dashboard, `/+stats` for usage, the PEP 691 simple
endpoints for package data, and the `inspect` endpoints for archive contents. Anything the UI shows, a script can
fetch the same way.

## Tradeoffs

- **One process, local state.** No replication, no failover. A cache instance per site or cluster is the intended
  shape; each warms independently.
- **The first request for anything pays upstream latency.** Streaming removes the buffering penalty, not the
  network. A cold cache behaves like pypi.org plus one hop until it has seen your working set once.
- **redb has one writer.** Fine for an index server (reads dominate by orders of magnitude), wrong for a
  write-heavy workload.
- **Trust follows the hash.** velodex verifies artifacts against the digests the index page advertises. If an
  upstream lies about its own hashes, velodex caches the lie; it defends the transport, not the source.

## In practice

- See what it does under load: [performance and methodology](@/explanation/performance.md)
- Compose mirrors, locals, and overlays: [the index model](@/explanation/indexes.md)
- Run it: [getting started](@/tutorials/getting-started.md), [configuration](@/reference/configuration.md)
- Watch it: [monitoring](@/guides/monitor.md)
