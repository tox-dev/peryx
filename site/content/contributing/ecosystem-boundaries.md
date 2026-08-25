+++
title = "Ecosystem boundaries"
description = "Dependency, capability, configuration, test, and documentation ownership rules."
weight = 2
+++

Each ecosystem-specific type and behavior belongs to its `peryx-ecosystem-*` owner crate. The owner keeps its protocols,
schemas, settings, storage encodings, migrations, routes, client commands, defaults, fixtures, tests, benchmarks,
documentation, and vocabulary together.

## Shared-code boundary

Shared crates may carry an opaque `Ecosystem` ID or call a neutral trait. This rule covers `peryx`, `peryx-driver`,
`peryx-storage`, `peryx-http`, `peryx-web`, `peryx-ha`, and `peryx-ha-distributed`. Shared code cannot:

- import an ecosystem implementation;
- branch on a known ecosystem ID;
- parse or emit owner metadata, routes, media types, or settings;
- own an ecosystem migration, fixture, benchmark, command, or protocol test;
- repeat ecosystem vocabulary in shared code or shared documentation.

`peryx` links registrations to compose the executable. It cannot implement or special-case an owner's behavior.

## Contract ownership

`peryx-core` owns the opaque ID and neutral values. `peryx-driver` owns focused runtime and installation traits.
`peryx-ha` owns neutral distributed contracts. `peryx-plugin-registry` validates linked registrations, resolves owners
from configuration, and installs selected capabilities.

The project reserves `peryx-ecosystem-*` names for implementations. Shared composition infrastructure uses the
`peryx-plugin-registry` name and contains no protocol behavior.

Add a shared contract when its types and semantics make sense without owner vocabulary. Keep the trait in the owner
crate when a neutral form would still expose owner fields.

## Installation boundary

Owners receive bounded contexts instead of mutable process state:

- `CapabilityInstallContext` permits identity, discovery, and driver capability registration.
- `AuthInstallContext` permits authentication services and routes.
- `RuntimeInstallContext` permits selected-owner services, routes, search, and maintenance.
- `DistributedInstallContext` adds replicated-view application.

Extend the narrowest context for a new neutral capability. Do not pass `AppState` around the boundary.

`EcosystemDriver` returns the core `Ecosystem` ID. Optional behaviors use separate capability traits. An owner registers
the capabilities it supports; shared callers handle a missing capability instead of calling a no-op default.

## Configuration and activation

The executable links all shipped owners. Resolved indexes select active registrations during startup. The registry then
registers capabilities and runs bounded installers for those owners.

Resolved startup configuration is the sole activation input. An unselected owner creates no schema, migration, table,
route, service, job, metric, watcher, timer, or task. Unknown IDs, conflicting registrations, and invalid owner settings
fail before runtime installation.

## Add an owner

1. Create `peryx-ecosystem-NAME` and define its stable ID in that crate.
1. Keep settings, protocols, metadata, migrations, routes, fixtures, tests, and benchmarks in its tree.
1. Implement required neutral contracts and each supported optional capability.
1. Install behavior through the narrowest bounded context.
1. Export one `PluginRegistration` with a unique ID and priority.
1. Link the registration from the `peryx` composition root.
1. Put process and external-service tests in the owner's system package.
1. Add end-user documentation under `site/content/ecosystems/NAME/`.

Run the workspace checks:

```shell
just lint
just test
just coverage-native
```

## Review rules

- Shared code uses opaque IDs and neutral traits instead of owner imports or match arms.
- Shared persistence stores neutral records, digests, and bytes; the owner interprets its data.
- Shared availability code exchanges neutral operations and cannot parse owner records.
- Installation uses a bounded context and cannot mutate unrelated process state.
- Resolved configuration controls activation.
- Inactive owners allocate no runtime or persistence resources.
- Owner routes, fixtures, tests, client assertions, and benchmarks stay in the owner tree.
- Shared prose contains no owner protocol or schema terms.

Owner documentation lives under `site/content/ecosystems/`. Shared pages describe contracts and link to owner pages for
protocol details.
