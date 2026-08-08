+++
title = "Ecosystem boundaries"
description = "Dependency and ownership rules for ecosystem implementations."
weight = 2
+++

Shared crates may carry an opaque ecosystem ID. They must not know protocol routes, media types, settings, storage keys,
client commands, defaults, or vocabulary.

## Ownership

`peryx-core` owns ecosystem contracts, stable IDs, and neutral DTOs. `peryx-plugin-registry` indexes registrations and
rejects duplicates; it implements no ecosystem behavior. Neither crate depends on an ecosystem implementation.

`peryx-ecosystem-pypi` owns all Python package protocol and policy behavior. `peryx-ecosystem-oci` owns all OCI
distribution protocol and policy behavior. Ownership includes parsing, protocol DTOs, storage encoding, mirroring,
routes, client snippets, tests, and user docs.

The `peryx` and `peryx-bench` binaries are composition roots. They may link implementation crates. No shared library may
do so.

## Capabilities

The `peryx-core` `EcosystemPlugin` contract provides the stable ID, defaults, settings compiler, startup installation,
OpenAPI paths, and client snippets. Runtime work uses focused contracts for maintenance, replicated updates, and
mirroring.

Callers query capabilities before use. They skip work whose capability is absent.

## Configuration

Configuration follows this path:

1. Parse the opaque ecosystem ID.
1. Reject IDs absent from the linked registrations.
1. Build the neutral index fields.
1. Pass `[index.settings]` to the selected ecosystem implementation.
1. Store the compiled value without exposing its type.
1. Install the ecosystem implementation during startup.

Adding a setting changes only the implementation crate and its documentation.

## Adding an ecosystem

1. Create `peryx-ecosystem-<name>` and define its ID there.
1. Keep all protocol code and tests in that crate.
1. Implement only supported capabilities.
1. Submit an implementation registration.
1. Link the crate from each binary that needs it.
1. Add docs under `site/content/ecosystems/<name>/`.

A shared-crate match arm, concrete import, or protocol term signals a broken boundary. Add a neutral contract only when
another implementation can use it without translation.

## Documentation

Core docs cover shared configuration and behavior. Protocol rules and client commands belong under
[PyPI](@/ecosystems/pypi/_index.md) or [OCI](@/ecosystems/oci/_index.md). Core pages link to those rules instead of
copying them.
