+++
title = "When to use peryx"
description = "Decide whether a read-through cache and private repository fit your artifact workflow."
weight = 0
+++

peryx sits between artifact clients and their upstream repositories. It caches public content, hosts private content,
and combines both behind one repository route.

## Good uses

### Shared CI caches

Clean CI workers often fetch the same immutable content. A peryx node near the workers fetches each item once, serves
later requests from local storage, and collapses concurrent misses into one upstream request.

Client setup and cache behavior depend on the ecosystem:

- [Cache Python packages in CI](@/ecosystems/pypi/guides/ci-cache.md)
- [Cache container images in CI](@/ecosystems/oci/guides/ci-cache.md)

### Private content with a public fallback

A [virtual repository](@/core/indexes.md) combines hosted and cached members. Hosted content can shadow an upstream
candidate with the same logical identity, which lets the server apply precedence before a client sees candidates.

Each ecosystem defines its candidate identity and client behavior:

- [Compose Python package overlays](@/ecosystems/pypi/guides/compose-overlays.md)
- [Run a private container registry](@/ecosystems/oci/guides/private-registry.md)

### Upstream outage tolerance

peryx can serve cached metadata and immutable content while an upstream is unavailable. New uncached content remains
unavailable until the upstream recovers.

### Restricted networks

A read-through cache can be the approved egress path for artifact traffic. An isolated network can use a prepared data
directory containing its working set.

- [Prepare Python packages for an air gap](@/ecosystems/pypi/guides/air-gapped.md)
- [Prepare container images for an air gap](@/ecosystems/oci/guides/air-gapped.md)

### Content deduplication

The content store keys immutable bytes by digest. Repositories that refer to the same bytes share one stored copy.
Ecosystem drivers preserve their protocol metadata and content relationships above that store.

### A focused artifact service

peryx serves the ecosystems in the [capability matrix](@/ecosystems/capabilities.md) from one process and data
directory. It fits teams that do not need the wider format catalog or workflow system of a general artifact manager.

## Poor fits

- Use an ecosystem mirror tool when you need its archival, delta, or public-mirror conventions. The PyPI guide explains
  the difference between a working-set cache and a
  [full Python package mirror](@/ecosystems/pypi/guides/private-mirror.md).
- Choose another repository service when the [capability matrix](@/ecosystems/capabilities.md) does not list the client
  protocol or operation you require.
- Use a build service when you need to compile or transform artifacts. peryx stores and serves artifacts; it does not
  build them.

## Next steps

- [Install and start peryx](@/core/getting-started.md)
- [Choose an ecosystem](@/ecosystems/_index.md)
- [Understand repository roles](@/core/indexes.md)
