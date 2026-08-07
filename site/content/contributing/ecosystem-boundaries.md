+++
title = "Ecosystem boundaries"
description = "The contracts, dependency rules, and ownership model for ecosystem implementations."
weight = 2
+++

An ecosystem is an implementation behind core-owned capability traits. Shared crates may carry an opaque ecosystem ID,
but they must not know a protocol's routes, media types, settings, storage keys, commands, defaults, or vocabulary.

## Crates

- `peryx-ecosystem-contract` contains neutral installation DTOs and reexports the core installer contract. It cannot
  depend on an ecosystem implementation.
- `peryx-ecosystem-registry` is the composition boundary. It links every implementation shipped in the binary, resolves
  configured IDs, and dispatches settings compilation and installation through plugins.
- `peryx-ecosystem-pypi` owns all Python package protocol behavior, metadata, storage encoding, mirroring, snippets,
  defaults, routes, and tests.
- `peryx-ecosystem-oci` owns all distribution-protocol behavior, manifests, storage encoding, mirroring, defaults,
  routes, and tests.
- `peryx-core`, `peryx-driver`, `peryx-storage`, `peryx-http`, `peryx-ha`, and `peryx-ha-distributed` depend only on
  neutral contracts. They never import either implementation crate.

The `peryx` binary depends on the registry, not on concrete ecosystem crates. This keeps one composition root without
letting implementation types leak into startup code.

## Capability model

`EcosystemPlugin` owns defaults, opaque settings compilation, installation, snippets, OpenAPI paths, and capability
declarations. `EcosystemDriver` covers request serving and route classification. Optional runtime work uses separate
traits:

- `MaintenanceDriver` scans references, reports usage, and performs implementation-owned maintenance.
- `ReplicatedApplyDriver` rebuilds implementation-owned derived state after replicated metadata changes.
- `MirrorDriver` plans, synchronizes, and verifies an implementation-owned mirror.
- Installer contracts register drivers, lexicons, OpenAPI paths, and other capabilities during startup.

Callers query the capability registry. An absent capability is unsupported, not a successful no-op.

## Configuration flow

1. Configuration parsing validates the ecosystem ID syntax.
1. The registry rejects IDs not installed in the binary.
1. Shared index fields become neutral index DTOs.
1. The registry passes each implementation-owned settings table to that plugin.
1. The plugin compiles it into an opaque value only that plugin can inspect.
1. Installers register capabilities for the resolved process state.

Ecosystem settings remain opaque TOML at the shared boundary. Adding a setting changes only its implementation and
documentation.

## Adding an ecosystem

1. Create `peryx-ecosystem-<name>` and define its ID in that crate.
1. Implement only the capabilities the ecosystem supports.
1. Keep protocol DTOs, storage keys, parsing, routes, defaults, snippets, mirror behavior, and tests in that crate.
1. Add its installer to `peryx-ecosystem-registry`.
1. Add user documentation below `site/content/ecosystems/<name>/`.

If a change to an ecosystem requires a match arm, concrete import, or protocol term in a shared crate, the boundary is
wrong. Extend a neutral trait or DTO only when another implementation can use the same concept without translation.

## Documentation ownership

Core documentation explains neutral concepts such as indexes, roles, policy, storage, configuration, and availability.
Protocol examples and client commands belong under [PyPI](@/ecosystems/pypi/_index.md) or
[OCI](@/ecosystems/oci/_index.md). Shared pages may link to those examples but should not duplicate their rules.
