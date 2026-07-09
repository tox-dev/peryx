+++
title = "From a self-hosted registry"
description = "Move off a self-hosted /v2/ registry (distribution's registry:2, Harbor, or similar) and map its pull-through cache and hosted repos onto velodex."
weight = 1
+++

You run your own `/v2/` registry: CNCF [distribution](https://distribution.github.io/distribution/)'s `registry:2` as a
Docker Hub pull-through cache and/or a private store, or [Harbor](https://goharbor.io/) wrapping the same core with
projects, replication, and scanning. You want velodex's single-flight fetch, one content-addressed blob store shared
across every index, and virtual shadowing, without running the registry, a database, and a job stack. This page maps a
`/v2/` setup onto velodex.

## What moves cleanly

velodex speaks the same [distribution spec](https://github.com/opencontainers/distribution-spec) your clients already
use, so the wire protocol does not change, only where it points and how it is configured. The pieces line up:

| Your setup                                                    | velodex                                                                            |
| ------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| `registry:2` with `proxy.remoteurl = https://registry-1...`   | a `cached` OCI index pointed at the same upstream                                  |
| `registry:2`/Harbor hosted repository (you push into it)      | a `hosted` OCI index with an `upload_token`                                        |
| Harbor project (namespace for a set of repos)                 | an index (a `route` prefix is the namespace)                                       |
| Harbor proxy-cache project                                    | a `cached` index                                                                   |
| Harbor replication rule pulling one registry into another     | a `cached` index, warmed on pull (no rule engine)                                  |
| A stack of the above served under one endpoint                | a `virtual` index with `layers = [...]` ([why](@/core/indexes.md))                 |
| `storage.filesystem.rootdirectory` / Harbor's registry volume | velodex's content-addressed blob store: one copy per digest, shared across indexes |
| `htpasswd` / Harbor robot account on a push repo              | one `upload_token` per hosted index; reads are open to velodex's network           |

## The config

A `registry:2` pull-through cache is a small YAML file with a `proxy` block; a private registry drops the `proxy` block
and gains `htpasswd` auth. Both collapse to `[[index]]` entries in one [velodex.toml](@/core/configuration.md). A Docker
Hub cache plus a hosted store:

```toml
# velodex.toml
[[index]]
name = "dockerhub"
route = "dockerhub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"

[[index]]
name = "images"
route = "images"
ecosystem = "oci"
hosted = true
upload_token = "<token>"
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
your image wins. That is the [dependency-confusion defense](@/core/glossary.md#shadowing) for containers. Point `cached`
at GHCR, ECR, or a Harbor `/v2/` the same way; any registry that implements the spec.

## What changes for clients

The route is a prefix on the image name. A `cached` index at route `dockerhub` serves Docker Hub's `library/alpine` as
`dockerhub/library/alpine`, because OCI names are content-addressed and velodex carries the index in the `<name>`:

```shell
docker pull 127.0.0.1:4433/dockerhub/library/alpine:latest
docker tag  myapp 127.0.0.1:4433/images/myapp:1.0
docker push 127.0.0.1:4433/images/myapp:1.0
```

There is **no bulk image import**. Images are content-addressed, so the cache repopulates itself: re-pull a tag through
velodex and the manifest and layers land on disk, deduplicated by digest. Migrating a pull-through cache means pointing
clients at the new endpoint; the first pull of each image warms it. For a private store, `docker push` your images into
the hosted index once; there is no registry-to-registry copy step to run. See the
[registry guide](@/ecosystems/oci/guides/container-registry.md) for the full pull/push walkthrough and
[compose overlays](@/ecosystems/oci/guides/compose-overlays.md) for wiring it into a stack.

## What velodex does not do

Be clear about the trade before you switch, especially coming from Harbor. velodex is a fast cache, host, and merge, not
a Harbor replacement:

- **No vulnerability scanning.** Harbor ships Trivy/Clair integration and can block a pull on a CVE. velodex does not
  scan images; run scanning in your pipeline or in front of velodex.
- **No project-level RBAC.** Harbor has users, roles, and per-project permissions. velodex has one `upload_token` per
  hosted index and open reads on its network; for per-team write control, issue a distinct hosted index and token per
  team.
- **No replication UI or rule engine.** Harbor's replication rules push and pull between registries on a schedule.
  velodex has no rule engine; a `cached` index warms itself on pull, and you run one instance per site.
- **No web-based user management.** There is no admin console for accounts, quotas, or robot tokens; configuration is
  the TOML file.

If those features are load-bearing, keep Harbor as the system of record and put velodex in front as a caching, shadowing
layer. If you were running `registry:2` for a pull-through cache and a private store, velodex covers both in one
process.
