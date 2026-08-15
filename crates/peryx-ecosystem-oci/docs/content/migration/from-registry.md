+++
title = "From a self-hosted registry"
description = "Move off a self-hosted /v2/ registry (distribution's registry:2, Harbor, or similar) and map its pull-through cache and hosted repos onto peryx."
weight = 1
+++

A self-hosted `/v2/` registry may use CNCF [distribution](https://distribution.github.io/distribution/)'s `registry:2`
as a [Docker Hub](https://hub.docker.com/) pull-through cache or private store. [Harbor](https://goharbor.io/) adds
projects, replication, and scanning. The mapping below replaces those registry routes with peryx indexes, shared blob
storage, and virtual shadowing.

## Compatible behavior

peryx implements the [distribution spec](https://github.com/opencontainers/distribution-spec), so clients keep the same
wire protocol. Update the endpoint and map the previous registry configuration as follows.

| Existing setup                                               | peryx configuration                                                                                               |
| ------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| `registry:2` with `proxy.remoteurl = https://registry-1...`  | Cached OCI index using the same upstream                                                                          |
| `registry:2` or Harbor hosted repository                     | Hosted OCI index with a write-granting `[[index.access_token]]`                                                   |
| Harbor project                                               | Index whose `route` provides the namespace                                                                        |
| Harbor proxy-cache project                                   | Cached index                                                                                                      |
| Harbor replication rule that pulls from another registry     | Cached index warmed on pull; peryx has no rule engine                                                             |
| Multiple repositories served from one endpoint               | Virtual index with `layers = [...]`; see [index composition](@/core/indexes.md)                                   |
| `storage.filesystem.rootdirectory` or Harbor registry volume | Content-addressed blob store shared across indexes                                                                |
| `htpasswd` or Harbor robot account on a push repository      | Write-granting `[[index.access_token]]` on each hosted index; reads remain open unless access rules restrict them |

## Configuration

A `registry:2` pull-through cache is a small YAML file with a `proxy` block; a private registry drops the `proxy` block
and gains `htpasswd` auth. Both collapse to `[[index]]` entries in one [peryx.toml](@/core/configuration.md). A Docker
Hub cache plus a hosted store:

```toml
# peryx.toml
[[index]]
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"

[[index]]
name = "images"
route = "images"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "upload"
secret = "<token>"
actions = ["write", "delete"]
```

To serve both under one name (your images shadowing Docker Hub, everything else falling through), stack them behind a
virtual index:

```toml
[[index]]
name = "all"
route = "all"
ecosystem = "oci"
layers = ["images", "dockerhub"]
```

A pull of `all/library/alpine` you have never published falls through to Docker Hub; once you push `all/library/alpine`,
your image wins. That is the [dependency-confusion defense](@/core/indexes.md#shadowing) for containers. Point the
`[[index.upstream]]` `url` at [GHCR](https://docs.github.com/packages), [ECR](https://aws.amazon.com/ecr/), or a Harbor
`/v2/` the same way; any registry that implements the spec.

## Client changes

The route is a prefix on the image name. A `cached` index at route `dockerhub` serves Docker Hub's `library/alpine` as
`dockerhub/library/alpine`, because OCI names are content-addressed and peryx carries the index in the `<name>`:

```shell
docker pull 127.0.0.1:4433/dockerhub/library/alpine:latest
docker tag  myapp 127.0.0.1:4433/images/myapp:1.0
docker push 127.0.0.1:4433/images/myapp:1.0
```

There is **no bulk image import**. Images are content-addressed, so the cache repopulates itself: re-pull a tag through
peryx and the manifest and layers land on disk, deduplicated by digest. Migrating a pull-through cache means pointing
clients at the new endpoint; the first pull of each image warms it. For a private store, `docker push` your images into
the hosted index once; there is no registry-to-registry copy step to run. See the
[registry guide](../guides/container-registry.md) for the full pull/push walkthrough and
[compose overlays](../guides/compose-overlays.md) for wiring it into a stack.

## Unsupported behavior

peryx provides caching, hosting, and virtual indexes. It does not replace these Harbor features:

- **No vulnerability scanning.** Harbor ships [Trivy](https://trivy.dev/)/[Clair](https://github.com/quay/clair)
  integration and can block a pull on a CVE. peryx does not scan images; run scanning in your pipeline or in front of
  peryx.
- **No project-level RBAC.** Harbor has users, roles, and per-project permissions. peryx has one write-granting
  `[[index.access_token]]` per hosted index and open reads on its network; for per-team write control, issue a distinct
  hosted index and token per team.
- **No replication UI or rule engine.** Harbor's replication rules push and pull between registries on a schedule. peryx
  has no rule engine; a `cached` index warms itself on pull, and you run one instance per site.
- **No web-based user management.** There is no admin console for accounts, quotas, or robot tokens; configuration is
  the TOML file.

If those features are required, keep Harbor as the system of record and use peryx as a caching, shadowing layer. Peryx
can replace a `registry:2` deployment used only for pull-through caching and private storage.
