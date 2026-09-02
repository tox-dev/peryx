+++
title = "Index settings"
description = "The [index.settings] table an OCI index reads: library_prefix and token_realms, their values, and what each one changes."
weight = 3
+++

The OCI implementation validates `[index.settings]` during startup. Unknown keys fail startup. OCI defines two settings:
`library_prefix` and `token_realms`.

## `library_prefix`

How a cached OCI index spells a repository name when it asks its upstream for it. Docker Hub keeps its official images
under the `library` namespace, so `docker pull ubuntu` resolves to `library/ubuntu`, and a client pulling through a
peryx route sends the short name it typed.

```toml
[[index]]
name = "hub"
route = "hub"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"

[index.settings]
library_prefix = "auto"
```

| Value    | Type   | Meaning                                                                           |
| -------- | ------ | --------------------------------------------------------------------------------- |
| `"auto"` | string | Prefix a single-segment name when the upstream is Docker Hub; this is the default |
| `true`   | bool   | Prefix a single-segment name for any upstream, including a Hub-compatible mirror  |
| `false`  | bool   | Send the repository name without rewriting it                                     |

Any other value fails at startup: `` `library_prefix` must be true, false, or "auto" ``.

### `auto` detection

`auto` reads the host of the index's `cached` URL and treats these three as Docker Hub:

- `docker.io`
- `index.docker.io`
- `registry-1.docker.io`

For any other host, including `ghcr.io`, Harbor, an Artifactory `/v2/` root, or self-hosted `distribution`, `auto`
leaves the repository name unchanged.

### Rewritten names

Only a single-segment repository name: `ubuntu` becomes `library/ubuntu`, `nginx` becomes `library/nginx`.

### Excluded names

- A multi-segment name, under every value of the setting. `grafana/grafana` and `library/nginx` already name their
  namespace, and prefixing one would ask for a repository that does not exist.
- Any name on a non-Hub upstream under `auto`.
- Any name at all under `false`.

### Rewrite scope

The upstream request, and both halves of it:

- The request path: `GET /v2/library/ubuntu/manifests/24.04`.
- The bearer token scope peryx asks the upstream's token realm for: `repository:library/ubuntu:pull`. A token issued for
  the scope the client typed would not authorize the pull of the rewritten repository, so the two agree.

Everything on peryx's side keeps the spelling the client used:

- The local cache keys for manifests, blobs, and tags.
- The tag list (`/v2/hub/ubuntu/tags/list` names `hub/ubuntu`).
- The referrers index.
- The name the image is served, listed, and browsed under, in the API and the [web UI](@/core/operations/web-ui.md).

`peryx mirror sync hub --option 'images=["ubuntu:24.04"]'` follows the same rule: it pulls `library/ubuntu` from Hub and
stores it as `ubuntu`.

## `token_realms`

The extra origins this index may present its upstream `username`/`password` pair to when a registry's `401` names a
token realm. The upstream's own origin is always trusted, so an index whose registry authenticates on its own host needs
no entry.

```toml
[[index]]
name = "hub"
route = "hub"
ecosystem = "oci"

[[index.upstream]]
name = "primary"
url = "https://registry-1.docker.io"
username = "robot"
password = "..."

[index.settings]
token_realms = ["https://auth.docker.io"]
```

A registry chooses the realm, so without this list a compromised or hostile upstream could name a collector of its own
and receive the credentials configured for it. peryx still contacts an unlisted realm — a public registry issues
anonymous pull tokens that way — but sends no credentials there, and it re-checks the origin at every redirect hop
rather than delegating that to the HTTP client.

Each entry is an origin: a scheme, a host, and an optional port, with no userinfo, path, query, or fragment. The
comparison is exact, so `https://auth.example` does not cover `http://auth.example`, `https://auth.example:8443`, or
`https://sso.auth.example`. Entries that are not well-formed origins fail startup, and trust is never learned from a
challenge.

Because the scheme is part of the origin, listing an `http://` entry is how an operator permits Basic credentials on the
wire in cleartext for that one destination. There is no exception for `localhost`: a development registry served over
`http` works because its own origin is the upstream, not because of its host.

Reaching a realm is a separate permission from receiving credentials. A realm on a private address is refused by the
outbound guard whatever this list says, until its host also appears in the upstream's
[`trusted_hosts`](@/core/operations/configuration.md).

| Entry                       | Effect                                                             |
| --------------------------- | ------------------------------------------------------------------ |
| `https://auth.docker.io`    | Docker Hub's separate authorization host receives the credentials  |
| `http://auth.internal:5001` | An internal realm receives them over cleartext, by operator choice |
| unlisted origin             | Contacted anonymously; the credentials stay home                   |

An unlisted realm that answers `401` reports which origin was refused the credentials, so the missing entry is named
rather than looking like a wrong password.

## Related

- The task, with the `true` and `false` cases:
  [mirror Docker Hub official images](@/ecosystems/oci/guides/hub-official-images.md)
- Why Hub needs the namespace, and what an upstream `401` means:
  [Docker Hub names and upstream auth](@/ecosystems/oci/hub-names-and-auth.md)
- Every other TOML key: [configuration](@/core/operations/configuration.md)
