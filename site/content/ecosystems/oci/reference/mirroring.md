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

## Failure handling

Anything the upstream governs is one error row naming the repository, the reference, and the reason, and the run carries
on: an image the registry does not have, a registry that refuses or cannot be reached, a manifest above the
four-megabyte ceiling, a connection that stops part way through a body, layer bytes that hash to something other than
the digest the manifest named. The references selected after it and the siblings scheduled beside it are independent
work and are mirrored regardless. A fault on peryx's own side is not the same thing. When the metadata store or the blob
store fails, every later `synced` and `cached` row would be a claim nothing has checked, so the run ends there and
reports the fault instead of finishing a report it cannot stand behind.

Both kinds reach the same call site while a layer is written, so the split is read off the failure rather than off where
it was raised. A stream that broke, and bytes that hash to something other than the digest the manifest named, describe
what the registry sent and leave the store intact. Every other fault the store reports is peryx's own, and a run that
meets one publishes no report at all.

The closing summary row states the verdict in its status column: `synced` when every selected reference is mirrored,
`partial` when some are and some failed, `error` when none are. Its reason carries the counts.

`sync` and `verify` exit non-zero whenever anything failed, a partial run included, so a zero exit always means the
mirror holds everything the run selected. That does cost a pipeline a failure when one blob out of many is missing,
which is the trade peryx takes: a mirror is asked for offline completeness, and a run that dropped content while exiting
zero would surface as an unexplained pull failure long afterwards. Read the error rows to decide what to retry, since
each names its own reference.

## Concurrency

A run overlaps the work one level of the manifest graph makes available rather than waiting out each descriptor in turn:
the platform manifests an index names move together, and so do the config and layers of an image manifest. The ceiling
is the index's [`upstream_concurrency`](@/core/operations/configuration.md), or three while the index is uncapped, which
is what containerd pulls at once by default. Selected images share that one budget, so one image with many layers costs
what many images with one layer cost.

The next level is scheduled only after every manifest above it has been parsed, which is what keeps the graph
deduplicated and the node and depth bounds exact: a digest two parents name is fetched once, and a graph over the bounds
still stops before the fetch. A layer two manifests share is transferred once and reported cached under the second. An
image and each sibling carries its own rows, so the report reads in selection then descriptor order however the
transfers finish.

## Repository scope

Manifest bytes are content-addressed, so two repositories mirroring the same image share one copy. The right to serve
them is recorded per repository. peryx answers a pull by digest only under a repository the manifest was recorded for,
and `verify` reads by the same rule. A digest another repository cached reports
`manifest not mirrored for this repository`, including a child manifest reached from an index, so a run never calls an
image ready for offline use that a pull rejects.

Mirroring the image under the second repository with `sync` records that membership and reuses the manifest and blobs
already stored, so nothing downloads twice.
