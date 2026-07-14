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

`peryx-web` points at no ecosystem. Server rendering calls the driver directly, and the browser fetches neutral
view-model JSON the server produced by calling the driver, so the client draws a PyPI project or an OCI manifest through
the same neutral models (see the block protocol below) without ever parsing a format itself.

## The layers

**Foundation.** The crates a driver reads through, none of which knows a wire protocol. `peryx-core` holds the neutral
domain: the closed set of packaging formats and the closed set of index roles (each is a Rust
[enum](https://doc.rust-lang.org/book/ch06-00-enums.html), a type whose value is exactly one of a fixed set of
variants), the per-ecosystem vocabulary, the neutral view models, and
[URL](https://developer.mozilla.org/en-US/docs/Learn/Common_questions/Web_mechanics/What_is_a_URL) path safety.
`peryx-index` is the role engine, covering the index model, route resolution, virtual-layer shadowing, and the serving
caches. `peryx-storage` is the two neutral stores, neither knowing any format's schema: artifacts as
[content-addressed](https://en.wikipedia.org/wiki/Content-addressable_storage)
[blobs](https://en.wikipedia.org/wiki/Object_storage) on disk (a file's key is the
[SHA-256](https://en.wikipedia.org/wiki/SHA-2) [hash](https://en.wikipedia.org/wiki/Hash_function) of its bytes, so
identical bytes are stored once and every reference is tamper-evident), and a [redb](https://github.com/cberner/redb)
(an embedded [key-value](https://en.wikipedia.org/wiki/Key%E2%80%93value_database) database, the Rust counterpart to
[SQLite](https://www.sqlite.org/) or [LMDB](http://www.lmdb.tech/doc/)) database holding the serial counter, the webhook
queue, and the shared key-value store each ecosystem lays its own metadata into.

The rest of the foundation is the cross-cutting machinery every ecosystem reuses. `peryx-search` is the package index,
built on [Tantivy](https://github.com/quickwit-oss/tantivy) (a
[full-text search](https://en.wikipedia.org/wiki/Full-text_search) library, the Rust counterpart to
[Lucene](https://lucene.apache.org/)). `peryx-events` carries [Prometheus](https://prometheus.io/)-format
[metrics](https://prometheus.io/docs/concepts/metric_types/), security events, and
[webhooks](https://en.wikipedia.org/wiki/Webhook) (an outbound HTTP callback fired when something changes);
`peryx-policy` the neutral allow/deny engine; `peryx-upstream` the neutral upstream transport — conditional GET, retry,
and range and streaming fetch — that each ecosystem's protocol layer builds on to reach the real *upstream* index peryx
proxies (such as [pypi.org](https://pypi.org/) or [Docker Hub](https://hub.docker.com/)); `peryx-identity` the neutral
access model — principals, project-glob grants, per-index ACLs, and its own token mint and verify — that both ecosystems
authorize through.

**The seam.** `peryx-driver` defines what an ecosystem plugs into. The word *seam* here is the software-design sense: a
place where you can change behavior by substituting a component rather than editing in place. The formal name for this
shape is a [Service Provider Interface](https://en.wikipedia.org/wiki/Service_provider_interface): the host defines an
interface, and providers (the ecosystems) implement it. That interface is the `EcosystemDriver`
[trait](https://doc.rust-lang.org/book/ch10-02-traits.html) (a trait is Rust's version of an interface: a set of methods
a type promises to provide). A mount declaration says where a driver's protocol lives in the URL space; the process
state carries the stores and caches a driver serves from; a standalone driver registry lets the binary's build and admin
paths reach a driver without spinning up the full serving state. Everything a driver needs to serve a request lives
here, and nothing about which ecosystems are installed.

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
([the step](<https://en.wikipedia.org/wiki/Hydration_(web_development)>) where client-side code attaches event handlers
to the already-rendered HTML so it becomes interactive), which runs as [WebAssembly](https://webassembly.org/) (Wasm, a
portable binary instruction format browsers execute at near-native speed) compiled from the same Rust. `peryx-web`
renders the neutral view models a driver produces.

**The composition root.** The `peryx` binary depends on everything, names the two ecosystems, and wires them in at
startup. [Composition root](https://blog.ploeh.dk/2011/07/28/CompositionRoot/) is the one place in a program that
assembles the concrete pieces; keeping it single means the rest of the code names no ecosystem. This is the only crate
that gets to know both formats at once.

## Core concepts

**Ecosystem.** A closed set in `peryx-core`, one entry per packaging format. Each maps to a fixed slot, so a driver
registry is a fixed-size array and dispatch is a [static match](https://doc.rust-lang.org/book/ch06-02-match.html)
rather than a runtime lookup. *Static dispatch* means the compiler resolves the call at build time; the alternative,
*dynamic dispatch* through a [trait object](https://doc.rust-lang.org/book/ch18-02-trait-objects.html), resolves it at
run time through a pointer. An ecosystem a request does not touch costs it nothing.

**Role.** How an index (a package [repository](https://en.wikipedia.org/wiki/Software_repository)) behaves: a *cached*
role [proxies](https://en.wikipedia.org/wiki/Proxy_server) an upstream, a *hosted* role accepts uploads, a *virtual*
role merges other indexes under one route. Every ecosystem gets all three roles from `peryx-index` for free. The product
`(role × ecosystem)` is the real unit of behavior: a cached PyPI index and a cached OCI index share the role engine and
differ in wire protocol.

**Index and resolution.** An index pairs a route with its kind and compiled policy. The router resolves a request path
to an index by [longest-prefix match](https://en.wikipedia.org/wiki/Longest_prefix_match) (the same rule an IP router
uses: the most specific configured route that the path starts with wins). A virtual index walks its layers in configured
order and merges their answers first-match, so an artifact in an earlier layer shadows a later one.

**The driver interface and its mount.** One trait carries the metadata every ecosystem declares — which ecosystem it
serves, where it mounts, how to classify a route for rate limiting, how to compile its policy — and the request-serving
behavior, split by mount. An *indexed* driver like PyPI serves the reads, uploads, and deletes the router routes to it
after resolving the index. An *absolute* driver like OCI owns a fixed top-level prefix (the root the distribution spec
mandates) and dispatches the whole request itself. Each driver implements only the half its mount uses.

**The two-part process state.** The state splits in two. The *serving state* holds the stores, caches, indexes, and
background handles a driver needs; a driver receives it as a shared, atomically reference-counted
[pointer](https://doc.rust-lang.org/std/sync/struct.Arc.html) (the way Rust hands one heap value to many tasks safely).
The *application state* wraps the serving state and adds the driver registry the router and
[rate limiter](https://en.wikipedia.org/wiki/Rate_limiting) reach. Because a driver never receives the registry, it
cannot reach another ecosystem's driver or enumerate them, and the compiler enforces that rather than a convention.
Handlers still read the serving state directly through the application state.

**The driver registry.** A standalone set of drivers keyed by ecosystem, which the composition root builds once. The
binary's config-build and admin commands never construct the full serving state, so they dispatch through this registry
to compile an index's policy or run a per-ecosystem admin scan without naming a format.

**Background maintenance.** The binary runs one process-wide minute ticker. It calls the cached-page refresh hook and
the idle-resource reclamation hook for each driver. PyPI refreshes stale upstream pages through the first hook; OCI
drops expired upload sessions and their staged files through the second. Use this scheduler to avoid per-resource tasks
and timers; keep resource state and cleanup rules in the ecosystem driver.

**Lexicon.** Each ecosystem's user-facing vocabulary, which its driver registers at install time. A surface localizes a
label from an index's ecosystem without the neutral core naming any format's words. PyPI calls a stored unit a
*project*; OCI calls it a *repository*; the lexicon holds that mapping so shared code stays neutral.

**The block protocol.** `peryx-web` renders a page from a list of neutral presentation blocks, an
[open set](https://doc.rust-lang.org/reference/attributes/type_system.html) of primitives keyed by shape (key-value,
chips, links, groups) rather than by format. This is the same idea as a server-driven UI: the server decides what blocks
to show, the client knows how to draw each block type. A driver turns its metadata into these blocks, so the UI gains an
ecosystem's page without a web-crate change.

## The serving path

The router resolves a request to an index and hands it to that index's driver, which reads through three neutral layers:
the store that holds artifacts and metadata, the cache that serves a warm copy from memory, and the upstream client that
fetches whatever is missing. The sections below take them in that order.

{% mermaid() %}
flowchart LR
req["request path"] --> router["peryx-http<br/>resolve index by longest prefix"]
router --> lookup["select driver<br/>by the index's ecosystem"]
lookup --> serve["driver serves the request<br/>reads shared serving state"]
serve --> state["stores · caches · indexes"]

class router accent
class serve good
{% end %}

An absolute-mount ecosystem skips the index resolution: the router mounts a catch-all under each prefix the driver
declares and hands it the whole request, which the driver resolves against the configured indexes itself.

### The storage layer

`peryx-storage` is the only crate that touches disk. It keeps two stores side by side under one data directory, so a
restart loses nothing and a backup is a directory copy. This layer and the two after it are each read the same way:
first the neutral **abstraction** every ecosystem shares, then the **extension** each format builds on top of it.

**Abstraction.** Two primitives are genuinely format-neutral. The **blob store** holds artifacts as ordinary files under
a [content-addressed](https://en.wikipedia.org/wiki/Content-addressable_storage) tree: each file is named for the
[SHA-256](https://en.wikipedia.org/wiki/SHA-2) of its bytes and sharded two levels deep (`sha256/ab/cd/abcd…`) so no
directory holds millions of entries. A write streams to a temporary file and
[atomically renames](https://man7.org/linux/man-pages/man2/rename.2.html) it into place once the hash is known, so a
reader never sees a half-written blob and two clients fetching the same artifact converge on one file. Because the name
is the hash, identical bytes are stored once across every ecosystem, and a truncated or tampered file is detectable. The
second neutral primitive is the **shared key-value store**, one [redb](https://github.com/cberner/redb) space of opaque
bytes the store never interprets: a driver owns its keys end to end, reads and writes values the store treats as blobs,
and enumerates them by ordered prefix scan. redb is an embedded, transactional
([ACID](https://en.wikipedia.org/wiki/ACID)) key-value store with one writer and
[MVCC](https://en.wikipedia.org/wiki/Multiversion_concurrency_control) readers that never block it, so a fetch reads
consistent state while a publish commits, and each write is one transaction.

A driver rarely writes one entry at a time. The store applies a batch of opaque entries in a single transaction — the
atomicity a cached page or a publish needs — and a journaled variant allocates the store's monotonic **serial** and
records an opaque changelog entry in the same commit, so a publish's entries, its serial, and its journal entry land
together or not at all. Nothing here is format-specific: the store holds content-addressed blobs, the shared key-value
store, the serial counter, and the durable webhook queue, and knows no ecosystem's schema.

**Extension.** Both ecosystems build their whole model on the shared key-value store, each under its own private
namespace, so the store never grows a table per format. PyPI keeps cached
[Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) pages, observed projects and
their [PEP 700](https://peps.python.org/pep-0700/) yank/hide status, hosted uploads and their overrides,
[PEP 658](https://peps.python.org/pep-0658/) metadata siblings, and the upstream URLs a cold blob refetches from —
committing a freshly fetched page in one batch, and a publish's entries, serial, and
[replication](https://en.wikipedia.org/wiki/Replication_%28computing%29) journal entry atomically. OCI keeps manifests
(byte-for-byte, so their digest stays stable), tags, cached tag-list pages, tag freshness, and the
[referrers](https://github.com/opencontainers/distribution-spec/blob/main/spec.md#listing-referrers) graph. peryx keeps
repository membership for OCI manifests and blobs in metadata. The shared content store tracks byte presence; the
membership map controls which `(index, repository)` may serve those bytes. peryx keeps artifact bytes in the shared blob
store, where an OCI layer and a Python wheel dedupe against the same content-addressed tree. Contributors can add a
third ecosystem without changing storage because each driver owns its value encodings and key layout.

OCI request handlers remove repository membership without unlinking bytes from the shared blob store. The
cross-ecosystem orphan collector unions each driver's references before it walks the blob tree, then performs a second
reference scan before unlinking candidates. Deferring reclamation bounds request memory. With the second scan, the
collector preserves bytes another ecosystem references.

OCI keeps upload sessions process-local because their staged blob disappears with the process. The session entry pairs
the staged state with its index resolution and full request name. peryx chooses a 128-bit random identifier, then checks
both values after authorizing a subsequent request. A writer may resume an upload that another credential opened for
that repository. Each body chunk passes the compiled file-size policy before `PendingBlob::write`; a transport error
reinserts the session at its accepted offset, while a policy denial drops the session and staged file.

{% mermaid() %}
flowchart TD
pypi["PyPI driver"]
oci["OCI driver"]
blob["content-addressed blob store<br/>shared · deduplicated"]
kv["shared key-value store<br/>opaque to the store · each driver owns its entries"]
pypi -->|"index pages · projects<br/>uploads · metadata"| kv
pypi -->|"artifact bytes"| blob
oci -->|"manifests · tags<br/>membership · referrers<br/>tag pages"| kv
oci -->|"layer bytes"| blob

class blob,kv accent
class pypi,oci good
{% end %}

The blob store and the shared key-value store in blue are neutral; the green drivers own the entries they lay into the
key-value store. Both drivers reach the blob store, and each keeps its own model in its own namespace of the key-value
store without the store ever reading it.

### The caching layer

`peryx-index` owns the coordination that lets a cold cache serve at upstream wire speed and a warm one from memory.

**Abstraction.** One `ServingCache`, shared by every ecosystem, carries five mechanisms:

- **Single-flight.** A per-key coalescing [map](https://doc.rust-lang.org/std/sync/struct.Mutex.html): when many clients
  request the same uncached artifact at once, one fetch runs upstream and the rest await it instead of each starting its
  own download. The name comes from Go's [singleflight](https://pkg.go.dev/golang.org/x/sync/singleflight) package; the
  effect is protection against a [thundering herd](https://en.wikipedia.org/wiki/Thundering_herd_problem).
- **Stale-on-error.** A bounded staleness window that lets a proxy serve the last good page when the upstream is
  unreachable, following [RFC 5861](https://datatracker.ietf.org/doc/html/rfc5861)'s `stale-if-error` (a close cousin of
  [stale-while-revalidate](https://web.dev/articles/stale-while-revalidate)). The bound is an operator's explicit
  choice, so a lasting outage surfaces as an error rather than as quietly ancient data.
- **The hot page cache.** A parsed, rewritten index page kept in memory in a [moka](https://github.com/moka-rs/moka)
  [cache](https://en.wikipedia.org/wiki/Cache_%28computing%29) bounded by a byte budget, so a warm request skips
  re-parsing. Every entry is re-derivable from the stored raw page, so evicting one costs hit rate, never correctness.
- **The negative cache.** Known-absent keys with a short expiry, so a flood of requests for a package that does not
  exist does not become a flood of upstream [404s](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/404).
- **The mutation epoch.** An [atomic](https://doc.rust-lang.org/std/sync/atomic/) counter advanced on every write.
  Derived state (the search index, hot page entries) records the epoch it was built at and rebuilds when the counter
  moves, so a publish becomes visible without invalidating each cache by hand.

**Extension.** The two ecosystems use different slices of that one cache. PyPI uses all five: it keys the hot page cache
by route, project, rendering, and epoch together, so the PEP 691 JSON, PEP 503 HTML, and legacy-JSON renderings of a
page share a raw fetch yet cache separately, and one epoch advance on a yank or upload retires every rendering at once.
It reads the negative cache on a miss and coordinates each upstream fetch through single-flight. OCI uses only the two
primitives that carry no assumption about an in-memory page: single-flight (keyed by blob digest or manifest reference)
and the stale bound. It caches nothing in the hot page or negative caches and never advances the epoch; its proxy cache
lives in the shared key-value store as cached tag pages and their freshness, and it gates freshness by comparing its own
stored fetch time through the same shared staleness window.

{% mermaid() %}
flowchart TD
pypi["PyPI"]
oci["OCI"]
flight["single-flight"]
stale["stale-on-error<br/>bounded staleness window"]
hot["hot page cache<br/>in memory · keyed by rendering + epoch"]
neg["negative cache"]
epoch["mutation epoch"]
kv["shared key-value store<br/>cached tag pages · freshness"]
pypi --> flight
pypi --> stale
pypi --> hot
pypi --> neg
pypi --> epoch
oci --> flight
oci --> stale
oci --> kv

class flight,stale accent
class hot,neg,epoch good
class kv warn
{% end %}

Blue marks the two primitives both ecosystems share; green marks the in-memory caches only PyPI uses; orange marks the
persistent key-value store OCI reaches for instead.

### The upstream layer

`peryx-upstream` is how a cached role reaches the real index it proxies.

**Abstraction.** The upstream client is a neutral HTTP client that names no format. It fetches a resource
[conditionally](https://developer.mozilla.org/en-US/docs/Web/HTTP/Conditional_requests): it sends the stored
[`ETag`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/ETag) as
[`If-None-Match`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/If-None-Match) and returns the raw status,
validator, serial, and freshness lifetime to the caller rather than acting on them, so a
[`304 Not Modified`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/304) or a `404` is a value the caching
layer interprets, not a branch buried in the client. A transient failure retries up to twice (three attempts total) on a
[5xx](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status#server_error_responses), `408`, or `429`, and on a
timeout or connection error, with [exponential backoff](https://en.wikipedia.org/wiki/Exponential_backoff) and
[jitter](https://aws.amazon.com/builders-library/timeouts-retries-and-backoff-with-jitter/) (100 ms base, 2 s cap).
Bounding how many fetches run at once lives one layer up, in the driver's upstream limiter: an optional per-index
[semaphore](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html), off by default, that caps simultaneous
upstream requests and applies [back-pressure](https://en.wikipedia.org/wiki/Flow_control_%28data%29). A waiter blocks up
to 30 s, then gets a retryable rate-limit error.

**Extension.** Both ecosystems fetch through the same client and the same limiter; the format-specific part is only what
they request and how they read the reply. PyPI fetches Simple-index pages (conditional on the index's etag and serial)
and wheels; OCI fetches manifests, blobs, and tag lists. Neither the client nor the limiter names a format.

{% mermaid() %}
flowchart TD
caller["cached role needs a page / blob"]
limit["upstream limiter<br/>per-index semaphore · off by default"]
client["upstream client<br/>conditional fetch"]
up["upstream index<br/>pypi.org · Docker Hub"]
retry{"transient?<br/>5xx · 408 · 429 · timeout"}
decide["caching layer decides<br/>status returned as-is"]
caller --> limit
limit -->|"permit"| client
limit -.->|"full 30s → retryable rate-limit error"| bp["back-pressure"]
client --> up
up -->|"304 / 200 / 404"| decide
up --> retry
retry -->|"≤2 retries · jittered backoff"| client

class client,up accent
class limit good
class retry,bp warn
{% end %}

## Cross-cutting concerns

Four more layers run alongside the serving path rather than on it: full-text search across every index, the policy
engine that gates what may be served or published, the identity model that decides who may act, and the events that make
the server observable. Each stays neutral, and each ecosystem extends it through the same seam.

### The search layer

`peryx-search` answers a substring query across every index in the process.

**Abstraction.** It is [full-text search](https://en.wikipedia.org/wiki/Full-text_search) on
[Tantivy](https://github.com/quickwit-oss/tantivy) (a Rust search engine, the counterpart to
[Lucene](https://lucene.apache.org/)) over a fixed neutral schema — display name, normalized name, route, ecosystem,
summary, and a free-text field — tokenized with an [n-gram](https://en.wikipedia.org/wiki/N-gram) tokenizer so a
fragment matches. The index records the [mutation epoch](#the-caching-layer) it was built at; when a write moves the
epoch, it does a full wipe-and-rebuild (delete every document, re-add, commit, reload) rather than an incremental
update, which keeps the indexing seam a single pure function of the current records. A query intersects the n-gram terms
(or runs a [regex](https://docs.rs/tantivy/latest/tantivy/query/struct.RegexQuery.html) for a `re:` or very short query)
and returns results ordered alphabetically by a composite sort key — not by
[relevance score](https://en.wikipedia.org/wiki/Okapi_BM25), a deliberate choice for a package index where the exact
name is what a user wants.

An authenticated search passes readable resource globs into the Tantivy query. Access applies before `Count` and
`TopDocs`, which prevents inaccessible totals and short pages. Public search stays unchanged.

**Extension.** Each ecosystem registers an indexer that maps its records into the neutral search document; a composite
indexer concatenates every ecosystem's documents into the one index. PyPI turns its projects into documents, OCI its
repositories. The schema and the query path never name a format; a driver owns only the record-to-document mapping.

{% mermaid() %}
flowchart TD
mut["mutation<br/>publish · yank · fresh upstream page"]
epoch["mutation epoch advances"]
check{"index epoch<br/>&lt; current?"}
comp["composite indexer<br/>each driver: records → search document"]
tan["Tantivy index<br/>neutral schema · n-gram tokens"]
q["search query"]
res["results<br/>alphabetical by sort key"]
mut --> epoch
epoch --> check
check -->|"stale → full wipe + rebuild"| comp
comp --> tan
q --> tan
tan --> res

class tan accent
class comp good
{% end %}

### The policy layer

`peryx-policy` decides whether an artifact may be served, cached, or uploaded. It is the cleanest example of the
abstraction-plus-extension shape: a neutral engine that an ecosystem feeds typed rules.

**Abstraction.** A compiled policy carries format-agnostic controls (allow and block project lists with
[PEP 503](https://peps.python.org/pep-0503/)-normalized keys, a maximum file size, a maximum project size) plus an
ordered list of artifact rules ([trait objects](https://doc.rust-lang.org/book/ch18-02-trait-objects.html)) the
ecosystem supplies. Evaluation is first-match: it checks the project lists (allow-list before block-list), then file
size, then each rule in order, and the first denial wins. A denial carries the action, the offending project, stable
rule and field identifiers, and a human reason, which maps to
[`403 Forbidden`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/403) with the serialized denial as the JSON
body.

**Extension.** Policy compilation is the seam. The binary splits an index's policy table: neutral keys go to the neutral
engine, and whatever remains goes to the driver. The default driver accepts no extra keys, so an unknown field is an
error rather than a silent no-op. PyPI compiles its leftover keys into artifact rules: a version-specifier rule, a
package-type (sdist versus wheel) rule, and Python-tag and platform-tag wheel rules. OCI adds none today and runs on the
neutral controls alone. A new format contributes rules without the engine ever learning its vocabulary.

{% mermaid() %}
flowchart TD
toml["index policy table (TOML)"]
split["binary splits keys"]
neutral["neutral engine<br/>allow/block projects · size caps"]
driver["driver compiles its rules<br/>PyPI: version · package-type · wheel-tag rules"]
pol["compiled policy<br/>neutral checks + artifact rules"]
req["upload / cached-fetch / serve"]
eval["first-match:<br/>project → size → rules"]
ok["allow → proceed"]
deny["deny → 403 + denial JSON"]
toml --> split
split --> neutral
split --> driver
neutral --> pol
driver --> pol
req --> eval
pol --> eval
eval -->|"no rule fires"| ok
eval -->|"first denial"| deny

class neutral accent
class driver good
class deny warn
{% end %}

### The identity layer

`peryx-identity` decides who a request speaks as and whether that principal may act. It began as a single upload-token
predicate; it is now the neutral seam both ecosystems authorize through, and it names no wire protocol.

**Abstraction.** The model is a `Principal` (anonymous, or a named subject), an `Action` (`Read`, `Write`, `Delete`),
and a `Grant` pairing actions with project globs. An index carries an `IndexAcl`: its `anonymous_read` flag and its
named tokens, each with a secret, grants, and an optional expiry. Two calls do the work. `IndexAcl::identify` turns an
`Authorization` header into a `Principal` by matching the presented password against a live token.
`authorize(principal, acl, project, action)` answers the one access question and returns a typed `Denial` (unavailable,
unauthenticated, or forbidden) an ecosystem maps to its own status. The crate also mints and verifies peryx's
audience-bound JWTs through `Signer`, so the signing key and audience check remain identity state rather than protocol
state. The composition root rejects empty and whitespace-only signing keys before it constructs `Signer`; keep that
invariant at the identity boundary when adding another token protocol. Registry-wide decisions call `authorize_all`. It
requires an explicit `*` grant because existing projects cannot prove access to future projects. Exact token grants keep
registry resources separate from project glob matching.

`peryx-driver::access::ReadAccess` resolves credentials for neutral presentation paths; hydrated `/+ui` handlers and
Leptos server rendering prepare one per-index decision before calling a browse driver. That decision also filters each
project name the driver returns. Search converts the credential into native access patterns. New presentation reads must
cross this seam.

**Extension.** The neutral core knows `(Principal, IndexAcl, project, Action)` and nothing about how a client presented
itself. Each ecosystem reduces its wire protocol to those four before it calls in. PyPI reads a Basic
`__token__:<token>` header on its upload path and calls `authorize` with `Write` or `Delete`; OCI resource routes do the
same for a push or delete, and the Bearer realm parses an OCI `scope=repository:<name>:pull,push` string into a project
and action in the OCI crate. The registry catalog scope maps to an exact internal grant after the core confirms
all-project access on each index. Project globs cover both PyPI project names and OCI repository names without teaching
the core either wire format. Contributors keep wire formats in ecosystem crates; principals and grants belong in the
identity crate.

**Rate-limit identity.** Implement `rate_limit_principal` in each driver that accepts `Authorization`. Use the resolved
index position for an indexed mount and the driver's protection space for an absolute mount. Return a verified
`Principal`; the limiter hashes its named subject with a process-random `RandomState` and stores no identity. Invalid
and anonymous credentials share an address bucket. Keep protocol parsing and signature checks in the driver.
`RateLimiter` contains the bucket state and proxy trust boundary. Require socket metadata from a configured trusted
proxy before accepting forwarding headers. Walk `X-Forwarded-For` from the nearest hop and stop at the first address
outside the trusted networks; ignore values farther toward the client. Gate `X-Forwarded-Host` and `X-Forwarded-Proto`
with the same peer check before constructing an absolute discovery URL or authentication realm. Use the local or
socket-peer identity when metadata is missing or the trusted suffix is unusable. Preserve the empty-list fast path and
parse configured CIDRs before requests arrive.

{% mermaid() %}
flowchart TD
pypi["PyPI upload<br/>Basic __token__:tok"]
oci["OCI push / delete<br/>resolve_writable"]
bearer["OCI Bearer realm<br/>scope=repository:name:push"]
id["identify header to Principal"]
auth["authorize(principal, acl, project, action)"]
acl["IndexAcl<br/>anonymous_read · named tokens · grants"]
verdict["Ok · Denial (unavailable · unauthenticated · forbidden)"]
pypi --> id
oci --> id
bearer --> id
id --> auth
acl --> auth
auth --> verdict

class auth accent
class acl good
class bearer warn
{% end %}

### The events layer

`peryx-events` carries three streams, all format-neutral.

**Abstraction.** [Metrics](https://prometheus.io/docs/concepts/metric_types/): request-path code emits an event over an
[mpsc channel](https://docs.rs/tokio/latest/tokio/sync/mpsc/index.html) to a dedicated aggregator thread, off the hot
path, rendered at `/metrics` as [Prometheus](https://prometheus.io/) counters and gauges (no histograms). Security
events: structured [tracing](https://github.com/tokio-rs/tracing) records tagged as security events (an auth failure, a
rate-limit denial) for an audit sink. [Webhooks](https://en.wikipedia.org/wiki/Webhook): durable, signed outbound
callbacks. A change persists a delivery record to redb before any network call, so a crash never drops one
([at-least-once](https://www.cloudcomputingpatterns.org/at_least_once_delivery/) delivery); a single background worker
drains due deliveries in batches, POSTs with an [HMAC-SHA256](https://en.wikipedia.org/wiki/HMAC) signature plus event,
delivery, and timestamp headers, and on failure reschedules with exponential backoff (5 s, tripling each attempt, capped
at 300 s) up to five attempts before marking the delivery failed.

**Extension.** This layer stays neutral by construction: an ecosystem emits an event through the shared API rather than
defining its own stream. A publish or a yank from either driver becomes the same metric increment and the same webhook
payload shape; the event names a project or a repository as data, never as a type the events crate knows. Its
`security::actor` takes the `Principal` the identity layer already resolved rather than re-parsing a Basic header, so a
record names the same subject the access decision ran against, and a bearer credential that carries no username still
attributes the action.

{% mermaid() %}
flowchart TD
evt["domain event<br/>publish · yank · auth fail · rate-limit deny"]
mch["mpsc → aggregator thread<br/>off request path"]
met["/metrics<br/>Prometheus counters + gauges"]
sec["security events<br/>structured tracing records"]
enq["persist delivery record<br/>durable queue (redb)"]
wk["delivery worker<br/>batch drain · HMAC-SHA256 signed POST"]
sub["subscriber endpoint"]
done["delivered"]
evt --> mch
mch --> met
evt --> sec
evt --> enq
enq --> wk
wk -->|"2xx"| done
wk -->|"fail → backoff 5s…300s · ≤5"| enq
wk --> sub

class met accent
class enq good
class wk warn
{% end %}

## Adding an ecosystem

The seam turns a new format into a bounded checklist rather than a server change.

1. Create a `peryx-ecosystem-<name>` crate that depends on `peryx-driver` and the foundation crates it needs.
1. Add the format to the set of packaging formats in `peryx-core`, which sizes the driver registries.
1. Implement the driver interface: declare where it mounts, classify routes for rate limiting, and serve the half your
   mount uses. Turn cached metadata into presentation blocks for the web UI and compile artifact rules from the index's
   policy table.
1. Expose an install entry point that registers the driver, its search indexer, and its vocabulary.
1. Implement the admin operations your format needs (blob-reference scanning,
   [`fsck`](https://en.wikipedia.org/wiki/Fsck) as a filesystem-check-style consistency scan of the stored records,
   import, purge), which the binary's maintenance commands dispatch through the driver.
1. Wire the crate into the `peryx` binary: install it at startup and add the driver to the driver registry.

Nothing in `peryx-http`, `peryx-web`, or the other ecosystem changes.
