+++
title = "Architecture for contributors"
description = "Crate ownership, dependency direction, plugin contracts, and configuration-selected availability."
weight = 1
+++

peryx is organized around two extension seams: ecosystems and availability coordination. Both are selected from startup
configuration in one binary. Shared crates define contracts; implementation crates depend on and implement them.

## Crate map

| Crate | Ownership | | --- | --- | | `peryx-core` | Stable IDs and neutral domain DTOs | | `peryx-ecosystem-contract` |
Shared ecosystem configuration DTOs | | `peryx-driver` | Plugin, serving, maintenance, mirror, and replicated-apply
traits; process state | | `peryx-ecosystem-registry` | Composition of the implementations shipped in the binary | |
`peryx-ecosystem-pypi` | Python package protocol and related policy, metadata, storage, UI data, mirroring, and snippets
| | `peryx-ecosystem-oci` | Distribution protocol and related policy, metadata, storage, UI data, mirroring, and
settings | | `peryx-ha` | Coordinator, membership, topology, lease, and diagnostics contracts | | `peryx-ha-local` |
No-task local coordinator | | `peryx-ha-distributed` | Replication, membership, reconciliation, liveness, and
distributed metrics | | `peryx-storage` | Persistence and blob primitives without protocol policy | | `peryx-http` |
Protocol-neutral HTTP boundary and middleware | | `peryx` | CLI, configuration merge, and startup orchestration |

## Ecosystem startup

`peryx-ecosystem-registry` constructs one `EcosystemPlugin` object per shipped implementation. The binary uses those
objects to validate ecosystem IDs, collect defaults, compile settings into opaque values, build the driver set, merge
OpenAPI paths, and install runtime capabilities.

Settings remain opaque after compilation. The plugin that compiled a value is the only code that downcasts it. Shared
configuration can reject an invalid table but cannot inspect a protocol field.

Optional behavior is explicit. Core asks whether a plugin supports a capability such as catalog synchronization or
trusted publishing. It does not call default no-op hooks on a broad driver and does not compare IDs.

## Availability startup

Configuration chooses the coordinator:

- `none` installs the local coordinator and leaves distributed state unallocated.
- `dc` and `ha` initialize the distributed coordinator and only their required workers, routes, and metrics.

Do not add a Cargo feature or environment mode for this choice. Every release binary contains both implementations.
Disabled behavior must be absent from the request path and task graph, not guarded by a boolean checked on each call.

## Dependency rules

The dependency graph points inward:

1. Foundation crates define neutral data and persistence primitives.
1. `peryx-driver` and `peryx-ha` define extension traits.
1. Ecosystem and coordinator crates implement those traits.
1. Registry and binary crates compose implementations.

No shared crate may depend on `peryx-ecosystem-pypi` or `peryx-ecosystem-oci`. No HA crate may parse ecosystem records.
See [ecosystem boundaries](@/contributing/ecosystem-boundaries.md) for the review checklist.

## Tests

Test behavior through public contracts. Protocol fixtures and wire assertions belong in the owning ecosystem crate.
Shared-crate tests use neutral IDs and records. Application tests cover composition, configuration dispatch, and
cross-cutting behavior without inspecting opaque plugin settings.

Required gates are the workspace build, tests, clippy with warnings denied, dead-code and dead-public checks, 100% line
and function coverage, frontend tests, and the documentation build.
