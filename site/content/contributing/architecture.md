+++
title = "Code architecture"
description = "How the crates fit together: the foundation layer, the driver seam, the ecosystems that plug into it, and how to add one."
weight = 5
+++

The runtime [architecture](@/core/architecture.md) page follows one request through the process. This page is for the
developer changing that process: how the workspace splits into crates, which way the dependencies point, and what you
implement to add a packaging format.

One rule shapes the layout. A shared crate defines an abstraction and the functionality every ecosystem reuses; an
ecosystem crate owns the full implementation of its own format and nothing else. Adding OCI beside PyPI meant writing a
new `peryx-ecosystem-oci` crate against a trait, not editing the server that hosts it. A third ecosystem is the same
shape again.

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

Dependencies point down. Both ecosystems and both hosts (the router and the UI) depend on `peryx-driver`, the seam in
blue. Neither ecosystem depends on the router: `cargo tree -p peryx-ecosystem-pypi --edges normal -i peryx-http` prints
nothing. The ecosystems reference `peryx-http` only as a dev-dependency, so their integration tests can spin up a real
router without the normal build ever pointing an ecosystem back at its host. The binary at the top is the one place that
names `pypi` and `oci` and wires them together.

The dashed edge is the exception worth knowing: the UI's hydration island still calls one PyPI parse helper, so
`peryx-web` carries a normal dependency on `peryx-ecosystem-pypi`. Server-side rendering goes through neutral models
(see the block protocol below); the client-side data fetch has not moved yet.

## The layers

**Foundation.** The crates a driver reads through, none of which knows a wire protocol. `peryx-core` holds the neutral
domain: the `Ecosystem` and `Role` enums, the `Lexicon`, the `UiBlock` view models, and URL path safety. `peryx-index`
is the role engine, covering `Index`, `IndexKind`, route resolution, virtual-layer shadowing, and the serving caches
(single-flight, the transformed-page cache, stale-on-error bounds). `peryx-storage` is the two stores (redb metadata,
content-addressed blobs on disk); `peryx-search` the package index; `peryx-events` metrics, security events, and
webhooks; `peryx-policy` the neutral allow/deny engine; `peryx-upstream` the fetch client; `peryx-identity` the upload
token check.

**The seam.** `peryx-driver` defines what an ecosystem plugs into. The `EcosystemDriver` trait is the whole contract;
`RouteMount` says where a driver's protocol lives in the URL space; `AppState` and `ServingState` carry the process
state; `DriverSet` lets the binary's build and admin paths reach a driver without an `AppState`. Everything a driver
needs to serve a request lives here, and nothing about which ecosystems are installed.

**The ecosystems.** `peryx-ecosystem-pypi` and `peryx-ecosystem-oci` each implement one `EcosystemDriver`. They read the
foundation crates through the seam and hold every format-specific decision: the Simple API and the distribution spec,
wheel and manifest parsing, the artifact rules each format adds on top of neutral policy.

**The hosts.** `peryx-http` resolves a request to a configured index and hands it to that index's driver; it names no
ecosystem. `peryx-web` renders the neutral view models a driver produces.

**The composition root.** The `peryx` binary depends on everything, names the two ecosystems, and wires them in at
startup through `install`. It is the only crate that gets to know both formats at once.

## Core concepts

**Ecosystem.** A closed enum in `peryx-core`, one variant per packaging format. Each variant maps to a `slot()` index,
so a driver registry is a fixed-size array and dispatch is a static match rather than a runtime lookup. An ecosystem a
request does not touch costs it nothing.

**Role.** How an index behaves: `Cached` proxies an upstream, `Hosted` accepts uploads, `Virtual` merges other indexes
under one route. Every ecosystem gets all three roles from `peryx-index` for free. The product `(role × ecosystem)` is
the real unit of behavior: a cached PyPI index and a cached OCI index share the role engine and differ in wire protocol.

**Index and resolution.** An `Index` pairs a route with its `IndexKind` and compiled policy. The router resolves a
request path to an index by longest route prefix. A virtual index walks its layers in configured order and merges their
answers first-match, so an artifact in an earlier layer shadows a later one.

**EcosystemDriver and RouteMount.** One trait carries the metadata methods every ecosystem shares (`ecosystem`, `mount`,
`classify_route`, `compile_policy`) and the serving methods split by mount. An `Indexed` driver like PyPI implements
`get`/`post`/`put`/`delete`, which the router calls after resolving the index. An `Absolute` driver like OCI owns a
fixed top-level prefix (`/v2/`) and implements `serve`, dispatching the whole request itself. Each driver implements
only the half its mount uses.

**AppState and ServingState.** The state splits in two. `ServingState` holds the stores, caches, indexes, and background
handles a driver needs; a driver receives it as an `Arc<ServingState>`. `AppState` wraps that in an `Arc` and adds the
driver registry the router and rate limiter reach. Because a driver never receives the registry, it cannot reach another
ecosystem's driver or enumerate them, and the compiler enforces that rather than a convention. `AppState` derefs to
`ServingState`, so handlers read `app.meta` unchanged.

**DriverSet.** A standalone registry of drivers keyed by ecosystem, which the composition root builds once. The binary's
config-build and admin commands never construct an `AppState`, so they dispatch through this to compile an index's
policy or run a per-ecosystem admin scan without naming a format.

**Lexicon.** Each ecosystem's user-facing vocabulary, which its driver registers at install time. A surface localizes a
label from an index's ecosystem without the neutral core naming any format's words.

**The block protocol.** `peryx-web` renders a page from a `Vec<UiBlock>`, an open enum of presentation primitives keyed
by shape (key-value, chips, links, groups) rather than by format. A driver turns its metadata into these blocks, so the
UI gains an ecosystem's page without a web-crate change.

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

## Adding an ecosystem

The seam turns a new format into a bounded checklist rather than a server change.

1. Create a `peryx-ecosystem-<name>` crate that depends on `peryx-driver` and the foundation crates it needs.
1. Add the variant to the `Ecosystem` enum in `peryx-core`, which sizes the driver registries.
1. Implement `EcosystemDriver`. Set the `mount`, classify routes for rate limiting, and implement the serving half your
   mount uses. Turn cached metadata into `UiBlock`s for the web UI and compile artifact rules from the index's policy
   table.
1. Expose an `install(state: &mut AppState)` that registers the driver, its search indexer, and its lexicon.
1. Implement the admin methods your format needs (blob-reference scanning, `fsck`, import, purge), which the binary's
   maintenance commands dispatch through the driver.
1. Wire the crate into the `peryx` binary: call `install` at startup and add the driver to the `DriverSet`.

Nothing in `peryx-http`, `peryx-web`, or the other ecosystem changes.
