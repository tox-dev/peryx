+++
title = "Architecture"
description = "How one binary composes ecosystem implementations and selects availability behavior."
weight = 1
+++

Peryx ships one binary containing the PyPI, OCI, and distributed availability implementations. Startup configuration
selects the active indexes and sets `availability.mode` to `none`, `dc`, or `ha`.

## Ecosystems

`peryx-core` owns the ecosystem contracts, stable IDs, and neutral DTOs. Each ecosystem crate registers an
`EcosystemPlugin` implementation. `peryx-ecosystem-pypi` and `peryx-ecosystem-oci` own all protocol and policy behavior,
including settings, routes, storage encoding, maintenance, mirroring, and UI data.

`peryx-plugin-registry` indexes registrations and contains no PyPI or OCI behavior. Unknown ecosystem IDs fail
configuration validation. Capability absence skips the dependent work.

The HTTP layer authenticates and rate-limits requests before dispatch. Indexed protocols receive the resolved index.
Absolute protocols receive requests under their declared prefixes. The selected ecosystem implementation owns protocol
parsing and response construction.

See [indexes](@/core/indexes.md) for shared role semantics and [ecosystems](@/ecosystems/_index.md) for protocol
behavior.

## Storage

`peryx-storage` provides transactions, key-value records, archive decoding, and content-addressed blobs. It does not
interpret ecosystem records. Ecosystem implementations own metadata encoding and report referenced digests through
`peryx-core` contracts.

## Availability

Omitting `[availability]` or setting `mode = "none"` allocates no distributed state and starts no distributed task,
timer, watcher, or transport.

`peryx-ha` owns the availability traits. `mode = "dc"` and `mode = "ha"` select the `peryx-ha-distributed`
implementation. Its contracts use operations, authority keys, frontiers, placements, and topology snapshots. Ecosystem
implementations map their mutations onto those contracts; HA code does not parse protocol records.

The `peryx` binary projects configuration, mounts authenticated routes, and starts processes. The distributed crate owns
replica pull loops, consensus, copy planning, placement reconciliation, reclamation, transfer coordination, metrics, and
worker lifecycles.

## Dependencies

Shared crates depend on contracts. Implementation crates depend on shared crates. The `peryx` binary links the PyPI,
OCI, and distributed availability implementations into one executable.

See [ecosystem boundaries](@/contributing/ecosystem-boundaries.md) and [high availability](@/core/high-availability.md).
