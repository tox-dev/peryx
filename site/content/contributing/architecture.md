+++
title = "Architecture for contributors"
description = "Crate ownership, capability installation, activation, and availability lifecycle."
weight = 1
+++

Peryx ships one executable containing every shipped ecosystem owner and the distributed availability implementation.
Resolved startup configuration selects what to install. Builds do not remove implementations, and command-line flags do
not activate them.

An inactive ecosystem installs no driver, schema, migration, route, service, job, metric, watcher, timer, or task.
`availability.mode = "none"` creates no distributed table, listener, transport, queue, metric family, or worker.

## Dependency direction

Composition roots and implementations depend on shared contracts. Shared crates cannot depend on ecosystem
implementations.

- `peryx-core`: stable IDs and ecosystem-neutral values
- `peryx-driver`: serving state and focused capability traits
- `peryx-plugin-registry`: registration validation, owner selection, and capability installation
- `peryx-ha`: availability configuration, lifecycle traits, and neutral distributed contracts
- `peryx-ha-distributed`: ownership, placement, replication, reconciliation, transfer, and distributed workers
- `peryx-storage`: metadata transactions and content-addressed blob persistence
- `peryx-identity`: principals, grants, tokens, and identity-provider contracts
- `peryx-index`: index roles, virtual composition, and route resolution
- `peryx-policy`: admission and retention policy domains
- `peryx-pql`: read-only queries over shared domains
- `peryx-search`: neutral search storage and document-provider contracts
- `peryx-http`: shared middleware and capability dispatch
- `peryx-events`: metrics, security events, and webhook delivery
- `peryx-upstream`: guarded upstream clients, credentials, and source selection
- `peryx-archive`: bounded archive inspection
- `peryx-web`: shared server-rendered and browser UI
- `peryx-bench-core` and `peryx-bench`: neutral benchmark measurement and execution
- `peryx-test-support`: process harnesses for system packages
- `peryx-ecosystem-*`: one owner's implementation, settings, tests, benchmarks, fixtures, and documentation
- `peryx`: binary composition, configuration projection, startup, supervision, and shutdown

The binary links one registration from each shipped owner. Linking does not permit `peryx` to implement or branch on an
owner's behavior. An ecosystem system package may depend on its owner; shared runtime crates may not.

The project reserves `peryx-ecosystem-*` names for implementations. `peryx-plugin-registry` contains neutral composition
logic and no protocol vocabulary.

## Capability model

`peryx-core::Ecosystem` is an opaque stable ID. `peryx-driver::serving::EcosystemDriver` exposes that ID from a protocol
driver. Optional behavior uses focused traits such as `PolicyDriver`, `RetentionDriver`, `CacheDriver`, and
`ImportDriver`. Callers request the capability they need and handle its absence at the boundary.

Shared capability inputs contain neutral IDs, digests, opaque settings, and operation records. If a contract needs an
owner's schema or protocol terms, it belongs in the owner crate.

Installation uses four bounded contexts:

- `CapabilityInstallContext` registers identity, protocol classification, client discovery, and driver capabilities.
- `AuthInstallContext` registers authentication services and routes.
- `RuntimeInstallContext` registers runtime services, protocols, search providers, routes, and maintenance workers.
- `DistributedInstallContext` adds replicated-view application to runtime installation.

The contexts expose registration methods rather than mutable `AppState`. Extend the narrowest neutral context when a new
shared capability needs installation.

## Registration and activation

Each owner exports one `PluginRegistration`. A registration provides configuration compilation, capability registration,
bounded installers, routes, discovery, jobs, and optional distributed installation.

`PluginRegistry` validates the linked registrations before configuration resolution. It rejects an empty linked set,
duplicate IDs, conflicting priorities or operator commands, conflicting authentication fields, and mismatched driver
IDs.

Resolved indexes select the active owners. An explicit owner ID selects its registration. An omitted ID selects the
unique lowest numeric priority. Unknown IDs and tied defaults fail startup. Activation retains selected registrations;
registration and installation then populate runtime state.

## Availability selection

The `[availability]` configuration selects one mode from the same executable:

- `none`: no managed availability resources
- `dc`: distributed resources within one datacenter
- `ha`: distributed resources across datacenters

An omitted table resolves to `none`. `read_only = true` may reject writes in that mode but does not install availability
services. Distributed replication settings require a distributed mode; they do not select one.

## Availability lifecycle

Availability startup crosses four typed boundaries:

1. `AvailabilityAssembler::assemble` validates projected configuration and returns an `AvailabilityInstall`.
1. `AvailabilityRuntime::prepare` acquires resources and returns `PreparedAvailability`.
1. `PreparedAvailability::activate` transfers resource ownership to `ActiveAvailability`.
1. The process projects the selected role and neutral capabilities through `AvailabilityStateInstall`.

`peryx-ha` owns the lifecycle and neutral ownership, copy, placement, reclamation, topology, and operation contracts.
`peryx-ha-distributed` owns listeners, consensus, replica loops, transfer, reconciliation, telemetry, cancellation, and
shutdown. `peryx` projects configuration, mounts returned routes, supervises the active handle, and orders shutdown.

## Crate autonomy

Unit and integration tests stay under the owning crate's `tests/` tree. A `#[path]` module may give a unit test private
access without placing its body under `src/`. A test that starts the executable or an external service belongs in a
metadata-declared system package. System packages are test composition roots and cannot become runtime dependencies.

`just crate-contract PACKAGE OUTPUT` checks one non-system crate's targets, tests, source ownership, and exact line and
function coverage. The contract cannot use another crate's tests to cover its source. System and frontend source enters
the merged coverage contract through their dedicated lanes.

Run `just ecosystem-boundaries`, `just test-layout`, and `just lint-contracts` after changing dependencies,
registration, test ownership, or public APIs.
