+++
title = "Docker Hub names and upstream auth"
description = "Why Docker Hub official images live under library/, why registry-mirror mode never needed the prefix, and what an upstream 401 now tells an operator."
weight = 3
+++

Two behaviors of a Docker Hub proxy surprise people: short image names, and what happens when Hub says no. Both come
from the same place, Hub answering `401` for a repository it will not discuss.

## Why official images live under `library/`

Docker Hub namespaces every repository by its owner: `grafana/grafana` belongs to the `grafana` organization. The
curated set Docker maintains ([official images](https://docs.docker.com/docker-hub/official_images/)) belongs to an
organization too, named `library`, so `ubuntu` on the registry is `library/ubuntu`.

The short form is a client-side convenience. When you type `docker pull ubuntu`, the Docker daemon expands the reference
before it touches the network: no registry host means Docker Hub, no namespace means `library`, no tag means `latest`.
The registry protocol has no short names, only `library/ubuntu`.

## Why a routed proxy index has to expand it

peryx serves the container protocol under a route: `/v2/hub/ubuntu/manifests/latest`. The name that arrives is whatever
the client put after the route, and a client that would have expanded `ubuntu` against Docker Hub does not expand it
against `peryx.internal:4433`, because as far as it knows this is some registry that happens to hold a repository called
`hub/ubuntu`. peryx strips the route, is left with `ubuntu`, and that is the name it would pass upstream.

Hub answers `401` to a request for a repository named `ubuntu`, not `404`, because its auth layer runs before its
lookup: the token realm will not issue a pull token for a scope it does not recognize. So the failure of a routed pull
of a short name looks like an authorization problem, which is what made it confusing.

The [`library_prefix`](@/ecosystems/oci/reference/settings.md) setting makes peryx do the expansion the client skipped,
on the upstream request alone. `auto`, the default, recognizes a Hub upstream by its host and prefixes a single-segment
name.

## Why registry-mirror mode already worked

Point the Docker daemon at peryx with `registry-mirrors` and short names have always worked, with no setting involved.
The daemon resolves `ubuntu` to `library/ubuntu` as part of its own reference parsing, then sends that full name to the
mirror, which serves an empty route. peryx receives `library/ubuntu` and passes it upstream verbatim, because a name
with a namespace is never rewritten.

The two modes differ in who expands the name. In registry-mirror mode the daemon does it and peryx sees the result. In
routed mode nothing expands it, so peryx does. Both end up asking Hub for `library/ubuntu`.

## What an upstream `401` tells you

peryx used to fold an upstream `401` into "this member does not have it", which reached the client as
`MANIFEST_UNKNOWN`: a pull of an official image by its short name reported a missing manifest, when the real cause was
Hub refusing the request. Since [#108](https://github.com/tox-dev/peryx/issues/108), an upstream `401` surfaces as
itself:

```json
{
  "errors": [
    {
      "code": "UNAUTHORIZED",
      "message": "upstream registry refused authentication for this manifest"
    }
  ]
}
```

The status is `401`. A cached index asks no credentials of its own clients, so this `401` is peryx reporting its
upstream, not peryx challenging you. Read it as one of:

- The repository name reaching the upstream is not one it will serve. On a Hub proxy, check `library_prefix` and the
  spelling of the name.
- The index's upstream credentials (`username`, `password`, `token`) are wrong or expired.
- The account behind those credentials cannot see that repository.

A `404` still means absent, and still reaches the client as `MANIFEST_UNKNOWN` or `BLOB_UNKNOWN`. A `403` also counts as
absent, since a registry answers it for a repository it will not show anonymously, and a
[virtual index](@/core/indexes.md) walks on to its next member.

## Why a cached image survives an upstream `401`

A tag is mutable, so a cached index revalidates it upstream once its freshness window (`cache_ttl_secs`) elapses. If
that revalidation draws a `401`, peryx has failed to confirm the tag rather than learned that it is gone, and forgetting
the image over an expired token would take a working deployment down. So it serves the cached manifest and its blobs,
bounded by `max_stale_secs` past the freshness window, the same bound it applies when an upstream is unreachable (see
[configuration](@/core/configuration.md)).

An expired upstream credential degrades in a useful order: what is cached keeps pulling, what is not cached reports the
`401`, and the logs carry the upstream status. Only a pull with nothing in the cache to fall back on fails.
