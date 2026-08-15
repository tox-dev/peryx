+++
title = "Architecture"
description = "How configuration composes ecosystem owners, shared services, and availability."
weight = 1
+++

Peryx ships one executable with the shared runtime, all shipped ecosystem owners, and every availability mode.
Configuration selects the active owners and availability mode at startup.

## Startup sequence

The `peryx` executable is the composition root. It performs these steps before accepting traffic:

1. Parse and validate configuration.
1. Add each linked owner's plugin registration to the registry.
1. Resolve index owners and let each selected owner compile its settings.
1. Build an active registry from the selected registrations.
1. Register active capabilities and run active installers.
1. Build shared serving services.
1. Assemble and prepare the configured availability mode.
1. Mount shared, owner, and availability routes.
1. Bind the listener, then activate prepared resources.

Unknown owner IDs, ambiguous defaults, invalid settings, and missing capabilities stop startup before the listener
binds. An unselected owner adds no capability to the active registry, runs no installer, mounts no route, and starts no
work.

## State boundaries

`ServingState` contains request-path services: storage contracts, index descriptions, authorization, jobs, events, and
active owner or availability capabilities. Shared domain code receives `ServingState` or a narrower core trait.

`AppState` is the process boundary used by HTTP extraction and runtime assembly. Handlers borrow `ServingState` from it
before calling domain services. Owner crates register routes and capabilities; shared HTTP code does not interpret an
owner's protocol, metadata, or client behavior.

## Index composition

Each `[[index]]` selects an ecosystem owner by ID or uses the unique lowest-priority registration, then assigns an index
role. The registry passes the owner's opaque settings to that owner for validation and compilation.

With no explicit indexes, configuration collects defaults from linked registrations. Any explicit `[[index]]` replaces
that default topology.

Cached indexes read from an upstream, hosted indexes accept owner-defined writes, and virtual indexes compose an ordered
set of indexes owned by the same ecosystem. The [index model](@/core/indexes.md) defines shared rules. The
[ecosystem owner docs](@/ecosystems/_index.md) define protocol behavior and settings.

## Request and storage path

Shared middleware resolves the route and caller. The active registry dispatches through the capability registered for
that route. If an owner omits a browse, discovery, or operation capability, Peryx does not mount that service.

Storage verifies and deduplicates bytes by digest. Owner crates encode metadata and report referenced digests through
neutral traits. Shared storage and availability crates do not parse ecosystem records.

Placement records verified possession of a digest by a node or datacenter. The availability implementation reclaims
content after the owner's reference contract and shared retention rules permit it. `peryx-ha` defines these contracts,
`peryx-ha-distributed` coordinates distributed decisions, and storage performs atomic persistence.

## Availability selection

`[availability].mode` selects `none`, `dc`, or `ha` in the same executable.

- `none` skips availability assembly and starts no topology, heartbeat, replication, or reconciliation work.
- `dc` starts distributed resources within one datacenter.
- `ha` starts distributed resources across configured datacenters.

The executable owns startup and shutdown order. The selected availability implementation owns and joins its resources. A
read-only process setting controls mutation access; it does not choose an availability mode.

The [contributor architecture](@/contributing/architecture.md) maps these boundaries to crates.
[Availability modes](@/core/availability/high-availability.md) covers deployment behavior.
