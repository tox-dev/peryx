+++
title = "Mirroring"
description = "Select, sync, and verify OCI images."
weight = 8
+++

`peryx mirror` pulls each configured image's manifest and referenced blobs. A manifest list expands into its platform
manifests.

```toml
[index.prefetch]
images = ["library/alpine:latest", "library/nginx:1.27"]
```

`packages` remains an accepted alias for deployments created before the ecosystem-owned config split. New config should
use `images`.

Command-line overrides are TOML values interpreted by the OCI implementation:

```shell
peryx mirror plan root/oci --option 'images=["library/alpine:latest"]'
peryx mirror sync root/oci --option 'images=["library/alpine:latest","library/nginx:1.27"]'
peryx mirror verify root/oci --option 'images=["library/alpine:latest"]'
```

At least one configured or command-line image is required. Repository rewriting follows the index's
[`[index.settings].library_prefix`](@/ecosystems/oci/reference/settings.md).

## Manifest validation

Every manifest is checked against the schema its media type declares before it is stored. An image manifest needs
`schemaVersion: 2`, a config descriptor, and a layers array; an index needs `schemaVersion: 2` and a manifests array.
Either array may be empty, so an artifact manifest with no layers is accepted. A media type peryx models no schema for
is stored as it came and names no dependencies.

A body that breaks a rule is reported as an error row naming the rule, and neither the manifest nor its tag is cached.
`verify` applies the same check to what the store already holds, so a run never calls an image complete when its layers
could not be read.

## Concurrency

A run overlaps the work one level of the manifest graph makes available rather than waiting out each descriptor in turn:
the platform manifests an index names move together, and so do the config and layers of an image manifest. The ceiling
is the index's [`upstream_concurrency`](@/core/operations/configuration.md), or three while the index is uncapped, which
is what containerd pulls at once by default. Selected images share that one budget, so one image with many layers costs
what many images with one layer cost.

The next level is scheduled only after every manifest above it has been parsed, which is what keeps the graph
deduplicated and the node and depth bounds exact: a digest two parents name is fetched once, and a graph over the bounds
still stops before the fetch. A layer two manifests share is transferred once and reported cached under the second. An
image or a sibling the upstream refuses is one error row and leaves the work beside it running, and each selected image
carries its own rows, so the report reads in selection order however the transfers finish.

## Repository scope

Manifest bytes are content-addressed, so two repositories mirroring the same image share one copy. The right to serve
them is recorded per repository. peryx answers a pull by digest only under a repository the manifest was recorded for,
and `verify` reads by the same rule. A digest another repository cached reports
`manifest not mirrored for this repository`, including a child manifest reached from an index, so a run never calls an
image ready for offline use that a pull rejects.

Mirroring the image under the second repository with `sync` records that membership and reuses the manifest and blobs
already stored, so nothing downloads twice.
