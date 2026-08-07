+++
title = "Glossary"
description = "The neutral concepts shared by every ecosystem and availability mode."
weight = 6
+++

## Index {#index}

An **index** is a configured endpoint that resolves and serves artifacts. Every index combines one [role](#roles) with
one [ecosystem](#ecosystem). Some client communities call the same concept a registry or repository; shared peryx code
uses *index*.

## Index role {#roles}

An **index role** controls where content comes from:

- **cached** fetches from configured [upstreams](#upstream) and stores verified results;
- **hosted** is authoritative for content clients publish;
- **virtual** resolves an ordered list of same-ecosystem indexes behind one route.

Roles are independent of wire protocol. See [the index model](@/core/indexes.md).

## Shadowing {#shadowing}

**Shadowing** is virtual-index precedence. When more than one member can serve the same logical item, the earlier
authoritative member wins and lower-precedence candidates remain hidden. Ecosystem implementations define candidate
identity; the virtual-index engine applies the ordering.

## Ecosystem {#ecosystem}

An **ecosystem** defines a wire protocol, naming rules, metadata schema, storage encoding, client snippets, and optional
capabilities. The implementations shipped by peryx are documented under [Ecosystems](@/ecosystems/_index.md).

Core identifies an ecosystem with an opaque stable string. It does not enumerate implementations or interpret their
records.

## Ecosystem contract {#ecosystem-contract}

The traits and neutral DTOs that `peryx-core` exposes to ecosystem implementations. They cover registration, serving,
maintenance, mirroring, settings compilation, replicated updates, and discovery without defining protocol behavior.

## Plugin registry {#plugin-registry}

The neutral index of registrations submitted by implementation crates. It resolves IDs and rejects duplicates. The
registry implements no ecosystem behavior; the binary links the implementations.

## Capability {#capability}

An optional behavior an ecosystem implementation exposes through a `peryx-core` contract. Callers query capabilities
before starting dependent work and skip that work when the capability is absent.

## Upstream {#upstream}

An external source a cached index consults on a miss. Credentials used toward an upstream are separate from client
credentials presented to peryx.

## Artifact {#artifact}

Immutable bytes addressed by digest. Ecosystems decide how metadata names and groups artifacts. Shared storage verifies
and deduplicates bytes without interpreting their format.

## Authority {#authority}

The right to commit mutations for one logical object. In distributed modes an authority has a home and a monotonic
epoch. A stale owner cannot commit under an older epoch.

## Frontier {#frontier}

A monotonic serial proving how much ordered state a replica or derived view has applied. Reads never claim state beyond
the lowest required frontier.

## Placement {#placement}

Evidence that a node or datacenter holds verified bytes for a digest. Metadata replication and byte placement advance
independently and are reported separately.

## Disabled availability {#disabled-availability}

The state selected by omitted availability configuration or `mode = "none"`. Peryx allocates no distributed state and
starts no distributed task, timer, watcher, or transport.

## Distributed coordinator {#distributed-coordinator}

The `peryx-ha-distributed` implementation selected by `mode = "dc"` or `mode = "ha"`. It provides membership,
replication, reconciliation, liveness, leases, and distributed diagnostics through traits owned by `peryx-ha`.

## Related

- [Ecosystem boundaries](@/contributing/ecosystem-boundaries.md)
- [High availability](@/core/high-availability.md)
- [Capabilities](@/ecosystems/capabilities.md)
