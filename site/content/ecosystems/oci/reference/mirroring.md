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

Command-line `--image` values extend the configured list:

```shell
peryx mirror plan root/oci --image library/alpine:latest
peryx mirror sync root/oci --image library/alpine:latest --image library/nginx:1.27
peryx mirror verify root/oci --image library/alpine:latest
```

At least one configured or command-line image is required. Repository rewriting follows the index's
[`library_prefix` setting](@/ecosystems/oci/reference/settings.md).
