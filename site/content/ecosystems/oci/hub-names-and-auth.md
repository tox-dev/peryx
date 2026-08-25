+++
title = "Docker Hub names and upstream auth"
description = "Docker Hub library names, registry-mirror expansion, and upstream 401 responses."
weight = 3
+++

Docker Hub expands short official-image names and returns `401` when its token realm rejects a repository scope.

## Official-image namespace

Docker Hub namespaces every repository by its owner: `grafana/grafana` belongs to the `grafana` organization. The
curated set Docker maintains ([official images](https://docs.docker.com/docker-hub/official_images/)) belongs to an
organization too, named `library`, so `ubuntu` on the registry is `library/ubuntu`.

The short form is a client-side convenience. When you type `docker pull ubuntu`, the Docker daemon expands the reference
before it touches the network: no registry host means Docker Hub, no namespace means `library`, no tag means `latest`.
The registry protocol has no short names, only `library/ubuntu`.

## Routed-proxy expansion

peryx serves the container protocol under a route: `/v2/hub/ubuntu/manifests/latest`. The name that arrives is whatever
the client put after the route, and a client that would have expanded `ubuntu` against Docker Hub does not expand it
against `peryx.internal:4433`, because that host is a separate registry with a repository called `hub/ubuntu`. peryx
strips the route and would pass `ubuntu` upstream.

Hub answers `401` to a request for a repository named `ubuntu`, not `404`, because its auth layer runs before its
lookup: the token realm will not issue a pull token for a scope it does not recognize. So the failure of a routed pull
of a short name therefore looks like an authorization failure.

The [`library_prefix`](@/ecosystems/oci/reference/settings.md) setting makes peryx do the expansion the client skipped,
on the upstream request alone. `auto`, the default, recognizes a Hub upstream by its host and prefixes a single-segment
name.

## Registry-mirror behavior

When the Docker daemon uses peryx through `registry-mirrors`, short names need no peryx setting. The daemon resolves
`ubuntu` to `library/ubuntu` as part of its own reference parsing, then sends that full name to the mirror, which serves
an empty route. peryx receives `library/ubuntu` and passes it upstream verbatim, because a name with a namespace is
never rewritten.

The two modes differ in who expands the name. In registry-mirror mode the daemon does it and peryx sees the result. In
routed mode nothing expands it, so peryx does. Both end up asking Hub for `library/ubuntu`.

## Upstream `401` diagnosis

Earlier versions folded an upstream `401` into "this member does not have it", which reached the client as
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

The status is `401`. A cached index asks no credentials of its own clients, so this response reports an upstream
rejection. Check these causes:

- The repository name reaching the upstream is not one it will serve. On a Hub proxy, check `library_prefix` and the
  spelling of the name.
- The index's upstream credentials (`username`, `password`, `token`) are wrong or expired.
- The account behind those credentials cannot see that repository.

A `404` still means absent, and still reaches the client as `MANIFEST_UNKNOWN` or `BLOB_UNKNOWN`. A `403` also counts as
absent, since a registry answers it for a repository it will not show anonymously, and a
[virtual index](@/core/repositories/indexes.md) walks on to its next member.

## Cached-image fallback

A tag is mutable, so a cached index revalidates it upstream once its freshness window (`cache_ttl_secs`) elapses. If
that revalidation returns `401`, peryx has failed to confirm whether the tag changed. It serves the cached manifest and
blobs within `max_stale_secs` past the freshness window, the same bound used when an upstream is unreachable (see
[configuration](@/core/operations/configuration.md)).

With an expired upstream credential, cached content remains available within `max_stale_secs`; uncached content returns
`401`, and logs include the upstream status.

## HTTPS token realms

The upstream `401` names its token realm in the `WWW-Authenticate` header, and peryx follows it to trade the index's
credentials for a bearer token. When those credentials are a `username`/`password` pair, peryx sends them as Basic
authentication, so a realm reached over plain `http` would put the secret on the wire in cleartext. A hostile or
compromised upstream that answers with `realm="http://attacker/token"` could harvest the mirror's credentials that way,
even over a TLS-valid connection to the registry itself.

peryx refuses to present Basic credentials to a token realm unless its scheme is `https`. The realm host is not
constrained: Docker Hub advertises `auth.docker.io`, a different host from the registry, and that stays valid. Every
token servers should use `https`. A loopback realm (`localhost`) is the only `http` exception because the credentials
remain on the machine.
