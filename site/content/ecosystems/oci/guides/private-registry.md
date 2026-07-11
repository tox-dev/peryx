+++
title = "Proxy a private registry"
description = "Cache a private or authenticated upstream registry (GHCR, ECR, Harbor, or an Artifactory /v2/) through peryx, supplying upstream credentials."
weight = 3
+++

A cached OCI index reads through to an upstream registry. When that upstream is private, peryx needs credentials to run
its bearer-token handshake; give them on the index and clients pull locally, presenting only peryx's own auth (or none).
This isolates the upstream secret in one process instead of on every developer's machine.

## The index

A cached OCI proxy is an `[[index]]` with `ecosystem = "oci"`, a `route`, and `cached` pointing at the upstream's
registry root. Add the credential fields the upstream expects:

```toml
# peryx.toml
[[index]]
name = "ghcr"
route = "ghcr"
ecosystem = "oci"
cached = "https://ghcr.io"
username = "<user>"
token = "<pat>"
```

peryx supports three credential fields on a cached index:

- `username` and `password`: Basic-auth credentials peryx presents when the upstream's `WWW-Authenticate` challenge asks
  for them.
- `token`: a bearer token, used directly. It takes precedence over `username`/`password` when both are set.

Which you set depends on the upstream:

- **[GHCR](https://docs.github.com/packages)**: `username = "<github-user>"`, `token = "<personal-access-token>"` (a PAT
  with `read:packages`).
- **[Amazon ECR](https://aws.amazon.com/ecr/)**: `cached = "https://<account>.dkr.ecr.<region>.amazonaws.com"`, with
  `username = "AWS"` and the password from `aws ecr get-login-password`. ECR tokens are short-lived; rotate the value on
  the schedule the token's lifetime demands.
- **[Artifactory](https://jfrog.com/artifactory/) or [Harbor](https://goharbor.io/)**: point `cached` at the registry's
  `/v2/` root and set `username`/`password`, or `token` if the server issues bearer tokens.

## Keep secrets out of the file

The credential fields hold literal strings, so a token in `peryx.toml` is a secret at rest. Restrict the file
(`chmod 600 peryx.toml`) and keep it out of version control. To avoid a plaintext token on disk, render the config from
a template at deploy time, injecting the value from a `PERYX`-scoped environment variable or a secret manager. See
[configuration](@/core/configuration.md) for the precedence tiers and how the file is loaded.

## Pull

Assume peryx runs at `127.0.0.1:4433`. [Docker](https://docs.docker.com/) and [Podman](https://podman.io/) trust a
loopback registry over plain HTTP with no setup, so on the same host a pull just works; `crane` and `podman` reaching it
take an insecure flag. Over the network, serve [TLS](@/core/serve-https.md) so clients need no flag at all.

Pull through peryx's route prefix; the upstream repository name follows it:

{% tabs(names="docker, podman, crane") %}

```shell
docker pull 127.0.0.1:4433/ghcr/<owner>/<image>:latest
```

%%%

```shell
podman pull --tls-verify=false 127.0.0.1:4433/ghcr/<owner>/<image>:latest
```

%%%

```shell
crane pull --insecure 127.0.0.1:4433/ghcr/<owner>/<image>:latest image.tar
```

{% end %}

peryx authenticates to the private upstream with the index's credentials, verifies each manifest and blob digest, stores
them, and serves them back. Clients never see the upstream secret; later pulls come from disk. Concurrent pulls of one
uncached layer share a single upstream fetch.

## When the upstream refuses

If the upstream rejects the index's credentials, the pull fails with `401` and the `UNAUTHORIZED` code, carrying a
message that names the upstream rather than reporting a missing manifest. A cached index asks no credentials of its own
clients, so read that `401` as the upstream refusing peryx: a wrong or expired `username`/`password`/`token`, an account
that cannot see the repository, or a repository name the upstream will not serve.

An image already in the cache keeps pulling through it. A cached tag whose revalidation draws a `401` still serves stale
within `max_stale_secs`, so an expired credential degrades pulls of new images while the working set keeps running. See
[Docker Hub names and upstream auth](@/ecosystems/oci/hub-names-and-auth.md).

## Client-facing auth

The upstream credentials are separate from what clients present to peryx. A cached index does not require clients to
authenticate; anyone who can reach the route can pull through it. Restrict who reaches peryx at the network layer, or
front the cache with a [virtual index](@/core/indexes.md) when you need to combine it with a hosted store.

## Related

- What cached, hosted, and virtual mean for containers: [the OCI ecosystem](@/ecosystems/oci/_index.md)
- Serve trusted HTTPS so clients drop the insecure flag: [serve HTTPS](@/core/serve-https.md)
- The full walkthrough of a running registry: [run a container registry](@/ecosystems/oci/guides/container-registry.md)
