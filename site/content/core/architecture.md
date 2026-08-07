+++
title = "Architecture"
description = "How configuration selects ecosystem plugins and availability coordination without leaking either concern into shared services."
weight = 1
+++

peryx ships one binary. That binary contains every supported ecosystem and both availability coordinators. Startup
configuration decides which indexes and coordinator are active; there is no ecosystem or availability build variant.

## Runtime composition

Startup resolves each configured ecosystem ID through `peryx-ecosystem-registry`. The registry contains plugin objects,
not protocol behavior. Each plugin implements the shared `EcosystemPlugin` contract and supplies:

- its stable ID and default indexes;
- settings validation and opaque compiled settings;
- its serving, maintenance, replication-apply, mirror, and discovery capabilities;
- installation into the process state.

The shared configuration and server code never parse an ecosystem's settings or compare an ecosystem ID to choose
behavior. An unknown ID or unsupported capability fails during configuration validation.

## Request path

The HTTP boundary authenticates, rate-limits, and resolves the target index. It dispatches the request to that index's
`EcosystemDriver`. Indexed protocols receive a resolved index; absolute protocols receive the request under the
top-level prefixes they declared. The driver owns protocol parsing, status mapping, metadata, rendering, and storage
encoding.

All drivers use the same index roles:

- A cached index fetches from configured upstreams and stores verified bytes.
- A hosted index is authoritative for published content.
- A virtual index resolves an ordered set of same-ecosystem members.

See [the index model](@/core/indexes.md) for role semantics and the [ecosystem pages](@/ecosystems/_index.md) for wire
behavior.

## Shared storage

`peryx-storage` owns persistence primitives, transactions, and the content-addressed blob store. It does not interpret
ecosystem records. Drivers serialize their metadata under their own key namespaces and report referenced digests through
capabilities. This keeps schema changes and protocol migrations inside the owning ecosystem crate while still
deduplicating identical bytes across indexes.

## Availability selection

The omitted `[availability]` table and `mode = "none"` select `peryx-ha-local`. The local coordinator starts no peer
clients, listeners, timers, queues, watchers, or distributed metrics. Distributed state is allocated lazily only when
`mode = "dc"` or `mode = "ha"` selects `peryx-ha-distributed`.

The shared HA contracts operate on neutral operations, authority keys, frontiers, placements, and topology snapshots. An
ecosystem driver maps its mutations and replicated view updates onto those contracts. HA code never parses protocol
records.

## Dependency direction

Shared crates depend on contracts and neutral state. Ecosystem crates depend inward on those crates and implement the
contracts. Only `peryx-ecosystem-registry` links concrete implementations, and only the `peryx` binary invokes the
registry during startup.

The boundary rules and dependency checks are documented in
[ecosystem boundaries](@/contributing/ecosystem-boundaries.md). Availability internals are documented in
[high availability](@/core/high-availability.md).
