+++
title = "When to use peryx"
description = "Choose peryx for bounded caches, private artifact hosting, and multi-ecosystem operations."
weight = 3
+++

Peryx fits deployments that need one artifact service with shared storage, observability, and availability controls.
Ecosystem crates retain client protocol behavior.

## Good fits

### Shared CI cache

A cached index reduces repeated upstream transfers across jobs. Single-flight fetches collapse concurrent misses, and
the stale window keeps stored metadata available through a bounded upstream outage.

### Private artifacts with fallback

A hosted index accepts writes defined by its ecosystem contract. A virtual index places it ahead of a cached member, so
internal content can take precedence through one client endpoint.

### Restricted networks

Populate the cache before restricting egress, then use offline mode to prevent upstream access. Verify that every needed
metadata record and blob exists before closing the network boundary; a cold miss cannot repair itself offline.

### Shared storage and operations

The content-addressed store deduplicates equal bytes across indexes. Access controls, metrics, logs, backup, and
availability use shared services. Ecosystem crates implement protocol behavior.

## Poor fits

Choose an archival mirror when you need a complete upstream history or an ecosystem-specific delta format. Choose a
build service when artifacts must be compiled or transformed; peryx stores and serves accepted bytes. Do not select
peryx when the ecosystem guide omits a required client operation.

## Ecosystem guides

The [ecosystem guides](@/ecosystems/_index.md) provide client commands, endpoint examples, air-gap procedures, and
protocol limits.

## Next steps

- [Install and start peryx](@/core/getting-started.md)
- [Understand index roles](@/core/indexes.md)
- [Read the architecture](@/core/architecture.md)
