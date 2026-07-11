+++
title = "Code architecture"
description = "How the crates fit together: the foundation layer, the driver seam, the ecosystems that plug into it, and how to add one."
weight = 5
+++

The runtime [architecture](@/core/architecture.md) page follows one request through the process. This page is for the
developer changing that process: how the source splits into crates, which way the dependencies point, and what you
implement to add a packaging format. It assumes no prior Rust or packaging knowledge and links each term the first time
it appears.

peryx is written in [Rust](https://www.rust-lang.org/) and organized as a
[Cargo workspace](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html): one repository holding many
[crates](https://doc.rust-lang.org/book/ch07-01-packages-and-crates.html), where a crate is Rust's unit of compilation
and dependency (a library or a binary). Splitting a program into crates is how Rust enforces boundaries. A crate can
only call what another crate makes public, and the dependency graph between crates must form no cycles, so the layering
below is not a convention the compiler could let you break.

One rule shapes the layout. A shared crate defines an abstraction and the functionality every ecosystem reuses; an
ecosystem crate owns the full implementation of its own format and nothing else. Here an *ecosystem* is a packaging
format peryx serves: [PyPI](https://pypi.org/) (the Python Package Index, where Python libraries are published) and
[OCI](https://opencontainers.org/) (the Open Container Initiative, the standard behind the container images
[Docker](https://www.docker.com/) and [Podman](https://podman.io/) push and pull) today. Each speaks its own *wire
protocol*, the on-the-wire request and response format a client and server agree on. Adding OCI beside PyPI meant
writing a new `peryx-ecosystem-oci` crate against a trait, not editing the server that hosts it. A third ecosystem is
the same shape again.

## The crate map

{% mermaid() %}
flowchart TD
bin["peryx<br/>binary, composition root"]
http["peryx-http<br/>axum router"]
web["peryx-web<br/>Leptos SSR UI"]
driver["peryx-driver<br/>the ecosystem seam"]
pypi["peryx-ecosystem-pypi"]
oci["peryx-ecosystem-oci"]
foundation["foundation crates<br/>core · index · storage · search<br/>events · policy · upstream · identity"]

bin --> http
bin --> web
bin --> pypi
bin --> oci
http --> driver
web --> driver
pypi --> driver
oci --> driver
driver --> foundation
http --> foundation
web -.->|hydration helper| pypi

classDef accent fill:#0072B2,stroke:#0072B2,color:#ffffff
classDef good fill:#009E73,stroke:#009E73,color:#ffffff
classDef warn fill:#D55E00,stroke:#D55E00,color:#ffffff
class driver accent
class pypi,oci good
class bin warn
{% end %}

Dependencies point down: a crate at the tail of an arrow uses the crate at its head. Both ecosystems and both hosts (the
router and the web UI) depend on `peryx-driver`, the seam in blue. Neither ecosystem depends on the router. You can
prove that with [`cargo tree`](https://doc.rust-lang.org/cargo/commands/cargo-tree.html), the command that prints a
crate's dependency graph: `cargo tree -p peryx-ecosystem-pypi --edges normal -i peryx-http` prints nothing, because no
normal (non-test) dependency path leads from the PyPI crate back to the router. The ecosystems reference `peryx-http`
only as a
[dev-dependency](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#development-dependencies) (a
dependency compiled for tests, never for the shipped binary), so their integration tests can spin up a real router
without the normal build ever pointing an ecosystem back at its host. The binary at the top is the one place that names
`pypi` and `oci` and wires them together.

The dashed edge is the exception worth knowing: the UI's hydration island still calls one PyPI parse helper, so
`peryx-web` carries a normal dependency on `peryx-ecosystem-pypi`. Server-side rendering goes through neutral models
(see the block protocol below); the client-side data fetch has not moved yet.

## The layers

**Foundation.** The crates a driver reads through, none of which knows a wire protocol. `peryx-core` holds the neutral
domain: the `Ecosystem` and `Role` [enums](https://doc.rust-lang.org/book/ch06-00-enums.html) (a Rust enum is a type
whose value is exactly one of a fixed set of variants), the `Lexicon`, the `UiBlock` view models, and
[URL](https://developer.mozilla.org/en-US/docs/Learn/Common_questions/Web_mechanics/What_is_a_URL) path safety.
`peryx-index` is the role engine, covering `Index`, `IndexKind`, route resolution, virtual-layer shadowing, and the
serving caches. `peryx-storage` is the two stores: package metadata in [redb](https://github.com/cberner/redb) (an
embedded [key-value](https://en.wikipedia.org/wiki/Key%E2%80%93value_database) database, the Rust counterpart to
[SQLite](https://www.sqlite.org/) or [LMDB](http://www.lmdb.tech/doc/)) and artifacts (the actual package files, a
[blob](https://en.wikipedia.org/wiki/Object_storage) or binary object each) on disk under
[content-addressable storage](https://en.wikipedia.org/wiki/Content-addressable_storage): a file's key is the
[SHA-256](https://en.wikipedia.org/wiki/SHA-2) [hash](https://en.wikipedia.org/wiki/Hash_function) of its bytes, so
identical bytes are stored once and every reference is tamper-evident. `peryx-search` is the package index, built on
[Tantivy](https://github.com/quickwit-oss/tantivy) (a [full-text search](https://en.wikipedia.org/wiki/Full-text_search)
library, the Rust counterpart to [Lucene](https://lucene.apache.org/)). `peryx-events` carries
[Prometheus](https://prometheus.io/)-format [metrics](https://prometheus.io/docs/concepts/metric_types/), security
events, and [webhooks](https://en.wikipedia.org/wiki/Webhook) (an outbound HTTP callback fired when something changes);
`peryx-policy` the neutral allow/deny engine; `peryx-upstream` the client that fetches from an *upstream* (the real
index peryx proxies, such as [pypi.org](https://pypi.org/) or [Docker Hub](https://hub.docker.com/)); `peryx-identity`
the upload-token check.

**The seam.** `peryx-driver` defines what an ecosystem plugs into. The word *seam* here is the software-design sense: a
place where you can change behavior by substituting a component rather than editing in place. The formal name for this
shape is a [Service Provider Interface](https://en.wikipedia.org/wiki/Service_provider_interface): the host defines an
interface, and providers (the ecosystems) implement it. That interface is the `EcosystemDriver`
[trait](https://doc.rust-lang.org/book/ch10-02-traits.html) (a trait is Rust's version of an interface: a set of methods
a type promises to provide). `RouteMount` says where a driver's protocol lives in the URL space; `AppState` and
`ServingState` carry the process state; `DriverSet` lets the binary's build and admin paths reach a driver without an
`AppState`. Everything a driver needs to serve a request lives here, and nothing about which ecosystems are installed.

**The ecosystems.** `peryx-ecosystem-pypi` and `peryx-ecosystem-oci` each implement one `EcosystemDriver`. They read the
foundation crates through the seam and hold every format-specific decision: PyPI's
[Simple repository API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) and OCI's
[distribution spec](https://github.com/opencontainers/distribution-spec/blob/main/spec.md),
[wheel](https://packaging.python.org/en/latest/specifications/binary-distribution-format/) (Python's built-package
format, standardized in a [PEP](https://peps.python.org/), a Python Enhancement Proposal) and
[manifest](https://github.com/opencontainers/image-spec/blob/main/manifest.md) (an image's list of layers) parsing, the
artifact rules each format layers on top of neutral policy.

**The hosts.** `peryx-http` is the [HTTP](https://developer.mozilla.org/en-US/docs/Web/HTTP) server, built on
[axum](https://github.com/tokio-rs/axum) (a web framework) and [tokio](https://tokio.rs/) (the
[async](https://rust-lang.github.io/async-book/) runtime that drives concurrent I/O without a thread per request). It
resolves a request to a configured index and hands it to that index's driver; it names no ecosystem. `peryx-web` is the
web UI, built on [Leptos](https://leptos.dev/), a Rust UI framework that runs the same components in two places. On the
server it does **SSR** ([server-side rendering](https://developer.mozilla.org/en-US/docs/Glossary/SSR)): it produces
finished HTML so the first page load shows content without waiting on the browser. That HTML then needs **hydration**
([the step](https://developer.mozilla.org/en-US/docs/Glossary/Hydration) where client-side code attaches event handlers
to the already-rendered HTML so it becomes interactive), which runs as [WebAssembly](https://webassembly.org/) (Wasm, a
portable binary instruction format browsers execute at near-native speed) compiled from the same Rust. `peryx-web`
renders the neutral view models a driver produces.

**The composition root.** The `peryx` binary depends on everything, names the two ecosystems, and wires them in at
startup through `install`. [Composition root](https://blog.ploeh.dk/2011/07/28/CompositionRoot/) is the one place in a
program that assembles the concrete pieces; keeping it single means the rest of the code names no ecosystem. This is the
only crate that gets to know both formats at once.

## Core concepts

**Ecosystem.** A closed enum in `peryx-core`, one variant per packaging format. Each variant maps to a `slot()` index,
so a driver registry is a fixed-size array and dispatch is a
[static match](https://doc.rust-lang.org/book/ch06-02-match.html) rather than a runtime lookup. *Static dispatch* means
the compiler resolves the call at build time; the alternative, *dynamic dispatch* through a
[trait object](https://doc.rust-lang.org/book/ch17-02-trait-objects.html), resolves it at run time through a pointer. An
ecosystem a request does not touch costs it nothing.

**Role.** How an index (a package [repository](https://en.wikipedia.org/wiki/Software_repository)) behaves: `Cached`
[proxies](https://en.wikipedia.org/wiki/Proxy_server) an upstream, `Hosted` accepts uploads, `Virtual` merges other
indexes under one route. Every ecosystem gets all three roles from `peryx-index` for free. The product
`(role × ecosystem)` is the real unit of behavior: a cached PyPI index and a cached OCI index share the role engine and
differ in wire protocol.

**Index and resolution.** An `Index` pairs a route with its `IndexKind` and compiled policy. The router resolves a
request path to an index by [longest-prefix match](https://en.wikipedia.org/wiki/Longest_prefix_match) (the same rule an
IP router uses: the most specific configured route that the path starts with wins). A virtual index walks its layers in
configured order and merges their answers first-match, so an artifact in an earlier layer shadows a later one.

**EcosystemDriver and RouteMount.** One trait carries the metadata methods every ecosystem shares (`ecosystem`, `mount`,
`classify_route`, `compile_policy`) and the serving methods split by mount. An `Indexed` driver like PyPI implements
`get`/`post`/`put`/`delete`, which the router calls after resolving the index. An `Absolute` driver like OCI owns a
fixed top-level prefix (`/v2/`, the root the distribution spec mandates) and implements `serve`, dispatching the whole
request itself. Each driver implements only the half its mount uses.

**AppState and ServingState.** The state splits in two. `ServingState` holds the stores, caches, indexes, and background
handles a driver needs; a driver receives it as an
[`Arc<ServingState>`](https://doc.rust-lang.org/std/sync/struct.Arc.html) (an `Arc` is an atomically reference-counted
pointer, the way Rust shares one heap value across many tasks safely). `AppState` wraps that in an `Arc` and adds the
driver registry the router and [rate limiter](https://en.wikipedia.org/wiki/Rate_limiting) reach. Because a driver never
receives the registry, it cannot reach another ecosystem's driver or enumerate them, and the compiler enforces that
rather than a convention. `AppState` [derefs](https://doc.rust-lang.org/std/ops/trait.Deref.html) to `ServingState` (a
Rust mechanism that lets the wrapper be used wherever the inner type is expected), so handlers read `app.meta`
unchanged.

**DriverSet.** A standalone registry of drivers keyed by ecosystem, which the composition root builds once. The binary's
config-build and admin commands never construct an `AppState`, so they dispatch through this to compile an index's
policy or run a per-ecosystem admin scan without naming a format.

**Lexicon.** Each ecosystem's user-facing vocabulary, which its driver registers at install time. A surface localizes a
label from an index's ecosystem without the neutral core naming any format's words. PyPI calls a stored unit a
*project*; OCI calls it a *repository*; the lexicon holds that mapping so shared code stays neutral.

**The block protocol.** `peryx-web` renders a page from a `Vec<UiBlock>`, an
[open enum](https://doc.rust-lang.org/reference/attributes/type_system.html) of presentation primitives keyed by shape
(key-value, chips, links, groups) rather than by format. This is the same idea as a
[server-driven UI](https://www.judo.app/blog/server-driven-ui): the server decides what blocks to show, the client knows
how to draw each block type. A driver turns its metadata into these blocks, so the UI gains an ecosystem's page without
a web-crate change.

## How a request reaches a driver

{% mermaid() %}
flowchart LR
req["request path"] --> router["peryx-http<br/>resolve index by longest prefix"]
router --> lookup["driver_for(index.ecosystem)"]
lookup --> serve["driver.get / serve<br/>Arc&lt;ServingState&gt;"]
serve --> state["stores · caches · indexes"]

classDef accent fill:#0072B2,stroke:#0072B2,color:#ffffff
classDef good fill:#009E73,stroke:#009E73,color:#ffffff
class router accent
class serve good
{% end %}

An absolute-mount ecosystem skips the index resolution: the router mounts a catch-all under each prefix the driver
declares and hands it the whole request, which the driver resolves against the configured indexes itself.

## The storage layer

`peryx-storage` is the only crate that touches disk. It keeps two stores side by side under one data directory, so a
restart loses nothing and a backup is a directory copy.

The **metadata store** is a single [redb](https://github.com/cberner/redb) database: an embedded, transactional
([ACID](https://en.wikipedia.org/wiki/ACID)) key-value store with one writer at a time and
[MVCC](https://en.wikipedia.org/wiki/Multiversion_concurrency_control) readers that never block it, so a fetch reads
consistent state while a publish commits. It holds one table per record kind, each keyed by a short string:

- `index_document`: a cached upstream index page, keyed `index/project`.
- `artifact_source`: where a blob's bytes come from, so a cold download knows which upstream URL to pull.
- `metadata_sidecar`: the [PEP 658](https://peps.python.org/pep-0658/) `.metadata` companion of a wheel, so a resolver
  reads a distribution's `METADATA` without downloading the whole wheel.
- `projects` and `project_status`: a project's display name and its yank/hide state.
- `uploads` and `overrides`: hosted publish records and the yank/hide overrides layered on them.
- `webhook_delivery` and `webhook_due`: the durable queue behind signed webhook delivery.
- `journal`: an append-only log of mutations, the seam a future
  [replica](https://en.wikipedia.org/wiki/Replication_%28computing%29) reads.
- `driver_kv`: a per-ecosystem key-value escape hatch, where OCI keeps manifests that fit no PyPI-shaped table.

Each write is one transaction, so a record and its counters commit together or not at all.

The **blob store** holds the artifacts themselves as ordinary files under a
[content-addressed](https://en.wikipedia.org/wiki/Content-addressable_storage) tree: each file is named for the
[SHA-256](https://en.wikipedia.org/wiki/SHA-2) of its bytes and sharded two levels deep (`sha256/ab/cd/abcd…`) so no
directory holds millions of entries. A write streams to a temporary file and
[atomically renames](https://man7.org/linux/man-pages/man2/rename.2.html) it into place once the hash is known, so a
reader never sees a half-written blob and two clients fetching the same artifact converge on one file. Because the name
is the hash, a truncated or tampered file is detectable, and a client can verify exactly what it downloaded.

## The caching layer

`peryx-index` owns the coordination that lets a cold cache serve at upstream wire speed and a warm one from memory. Four
mechanisms share one `ServingCache`:

- **Single-flight.** A per-key [`Mutex`](https://doc.rust-lang.org/std/sync/struct.Mutex.html) map (`inflight`): when
  many clients request the same uncached artifact at once, one fetch runs upstream and the rest await it instead of each
  starting its own download. The name comes from Go's [singleflight](https://pkg.go.dev/golang.org/x/sync/singleflight)
  package; the effect is protection against a [thundering herd](https://en.wikipedia.org/wiki/Thundering_herd_problem).
- **The transformed-page cache** (`hot`). A parsed, rewritten index page kept in memory in a
  [moka](https://github.com/moka-rs/moka) [cache](https://en.wikipedia.org/wiki/Cache_%28computing%29) bounded by a byte
  budget, so a warm request skips re-parsing. Every entry is re-derivable from the stored raw page, so evicting one
  costs hit rate, never correctness.
- **The negative cache** (`negative`). Known-absent keys with a short expiry, so a flood of requests for a package that
  does not exist does not become a flood of upstream
  [404s](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/404).
- **The mutation epoch** (`epoch`). An [atomic](https://doc.rust-lang.org/std/sync/atomic/) counter bumped on every
  write. Derived state (the search index, hot entries) records the epoch it was built at and rebuilds when the counter
  moves, so a publish becomes visible without invalidating each cache by hand.

On top of these, *stale-on-error* lets a proxy serve the last good page for a bounded window when the upstream is
unreachable, following [RFC 5861](https://datatracker.ietf.org/doc/html/rfc5861)'s `stale-if-error` (a close cousin of
[stale-while-revalidate](https://web.dev/articles/stale-while-revalidate)). That bound is an operator's explicit choice,
so a lasting outage surfaces as an error rather than as quietly ancient data.

## Adding an ecosystem

The seam turns a new format into a bounded checklist rather than a server change.

1. Create a `peryx-ecosystem-<name>` crate that depends on `peryx-driver` and the foundation crates it needs.
1. Add the variant to the `Ecosystem` enum in `peryx-core`, which sizes the driver registries.
1. Implement `EcosystemDriver`. Set the `mount`, classify routes for rate limiting, and implement the serving half your
   mount uses. Turn cached metadata into `UiBlock`s for the web UI and compile artifact rules from the index's policy
   table.
1. Expose an `install(state: &mut AppState)` that registers the driver, its search indexer, and its lexicon.
1. Implement the admin methods your format needs (blob-reference scanning, [`fsck`](https://en.wikipedia.org/wiki/Fsck)
   as a filesystem-check-style consistency scan of the stored records, import, purge), which the binary's maintenance
   commands dispatch through the driver.
1. Wire the crate into the `peryx` binary: call `install` at startup and add the driver to the `DriverSet`.

Nothing in `peryx-http`, `peryx-web`, or the other ecosystem changes.
