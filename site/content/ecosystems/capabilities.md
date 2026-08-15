+++
title = "Capability contract"
description = "How owner capabilities enter shared runtime state."
weight = 5
+++

`peryx-plugin-registry` composes linked ecosystem owners. `peryx-core::Ecosystem` carries owner identity; focused traits
define behavior. Shared crates contain no owner-specific configuration, schema, metadata, routes, or wire types.

## Lifecycle

1. **Registration** records an owner's stable ID, configuration compiler, defaults, and installation hooks. It does not
   start work or mutate runtime state.
1. **Configuration** resolves each index to a registered owner and validates the owner's settings.
1. **Selection** retains only registrations referenced by valid indexes.
1. **Capability registration** adds the selected owners' focused traits to the driver set.
1. **Installation** runs selected-owner hooks with the state required by each phase. `AuthInstallContext` installs
   authentication and credential behavior. `RuntimeInstallContext` installs request, protocol, storage-facing, and
   maintenance capabilities. `DistributedInstallContext` installs distributed capabilities.
1. **Lookup** selects a capability by owner ID. Callers handle an absent optional capability as unsupported behavior,
   not as a no-op implementation.

The bounded contexts keep process-wide application state outside owner crates and let tests run each installation phase
without starting the server.

## Capability groups

| Group        | Shared traits                                                               | Purpose                                                               |
| ------------ | --------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| Registration | `EcosystemRegistration`, `EcosystemConfig`                                  | Stable identity, defaults, and settings compilation                   |
| Runtime      | `EcosystemRuntime`, `DistributedRuntime`                                    | Shared and distributed installation                                   |
| Request      | `EcosystemAuth`, `RateLimitPrincipal`, `ClientDiscovery`, `EcosystemBrowse` | Authentication and request surfaces                                   |
| Description  | `EcosystemOpenApi`, `EcosystemSnippet`                                      | Owner-generated API and client material                               |
| Protocol     | `IndexedProtocolDriver`, `AbsoluteProtocolDriver`                           | Dispatch after shared middleware resolves context                     |
| Operations   | `DriverSet`                                                                 | Maintenance, integrity, retention, cache, import, and mirror behavior |

`DriverSet` stores each optional capability by owner ID. Callers request one focused trait and handle its absence at the
boundary. Owner metadata journals remain private; distributed code sees only neutral operation traits.

## Absence

An owner that is linked but not referenced by configuration is not activated. It installs no routes, services, jobs,
schema, migrations, background tasks, or distributed behavior.

An active owner may omit an optional capability. The caller must return the operation's documented unsupported or
not-found result. Shared code must not infer support from an owner ID or substitute a no-op capability.

## Add a capability

Add a shared trait only when its inputs, outputs, and invariants require no owner vocabulary. Keep parsing, wire DTOs,
schema, metadata, and owner terms in the owner crate. Add composition tests for installation and absence.

Owner docs list supported capabilities and behavior:

{{ ecosystem_owner_links() }}
