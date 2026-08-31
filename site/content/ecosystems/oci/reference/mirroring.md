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
