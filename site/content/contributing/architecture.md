+++
title = "Architecture for contributors"
description = "Crate ownership, dependency direction, plugin contracts, and availability startup."
weight = 1
+++

Peryx ships one binary containing the PyPI, OCI, and distributed availability implementations. Startup configuration
selects the indexes and sets `availability.mode` to `none`, `dc`, or `ha`.

## Crate map

| Crate                   | Responsibility                                    |
| ----------------------- | ------------------------------------------------- |
| `peryx-core`            | Ecosystem contracts, stable IDs, and neutral DTOs |
| `peryx-driver`          | Shared process state and runtime orchestration    |
| `peryx-plugin-registry` | Registration indexing and duplicate checks        |
| `peryx-ecosystem-pypi`  | Python package protocol and policy                |
| `peryx-ecosystem-oci`   | OCI distribution protocol and policy              |
| `peryx-ha`              | Availability traits                               |
| `peryx-ha-distributed`  | Distributed replication and coordination          |
| `peryx-storage`         | Persistence and content-addressed blobs           |
| `peryx-http`            | Shared HTTP middleware and dispatch               |
| `peryx-bench-core`      | Benchmark measurement and reports                 |
| `peryx`                 | Configuration, linking, and startup               |

## Implementation startup

Each ecosystem crate submits an `EcosystemPlugin` registration. `peryx-plugin-registry` indexes the registrations and
rejects duplicate IDs or priorities. It implements no ecosystem behavior. The `peryx` binary links every implementation
crate so all registrations reach the executable.

Shared configuration passes `[index.settings]` to the selected ecosystem implementation as TOML. That implementation
validates the table and stores its compiled value behind an opaque type. Shared code cannot inspect that value.

Optional work has a capability trait. Callers skip the work when the selected implementation lacks that capability.

## Availability startup

`availability.mode` controls startup:

- `none` allocates no distributed state and starts no distributed task, timer, watcher, or transport.
- `dc` and `ha` construct `peryx-ha-distributed` and start the configured distributed services.

Do not add a Cargo feature or environment mode for this choice. Disabled availability must stay out of the request path
and task graph.

## Dependency rules

Dependencies point toward contracts. `peryx-core` owns ecosystem traits, and `peryx-ha` owns availability traits.
`peryx-ecosystem-pypi`, `peryx-ecosystem-oci`, and `peryx-ha-distributed` implement them. The `peryx` binary composes
these implementations.

A shared crate must not depend on `peryx-ecosystem-pypi` or `peryx-ecosystem-oci`. An HA crate must not parse ecosystem
records. CI enforces the implementation dependency rule with `.github/scripts/check-ecosystem-boundaries`.

See [ecosystem boundaries](@/contributing/ecosystem-boundaries.md) for the ownership checklist.

## Tests

Protocol fixtures and wire assertions belong to the owning ecosystem crate. Shared-crate tests use neutral IDs and
records. Application tests cover configuration and composition through public APIs.

CI requires build, test, clippy, dead-code, dead-public, line-coverage, function-coverage, frontend, and documentation
checks.
