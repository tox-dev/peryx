+++
title = "Index settings"
description = "The [index.settings] table an OCI index reads: library_prefix, its three values, and what each one rewrites."
weight = 3
+++

`[index.settings]` carries the settings an ecosystem defines for itself. peryx compiles the table against the ecosystem
of the index that owns it, and a key that ecosystem does not know fails at startup. PyPI defines no settings, so
`[index.settings]` on a PyPI index is a startup error; OCI defines one key, `library_prefix`, on a cached index.

## `library_prefix`

How a cached OCI index spells a repository name when it asks its upstream for it. Docker Hub keeps its official images
under the `library` namespace, so `docker pull ubuntu` resolves to `library/ubuntu`, and a client pulling through a
peryx route sends the short name it typed.

```toml
[[index]]
name = "hub"
route = "hub"
ecosystem = "oci"
cached = "https://registry-1.docker.io"

[index.settings]
library_prefix = "auto"
```

| Value    | Type   | Meaning                                                                                         |
| -------- | ------ | ----------------------------------------------------------------------------------------------- |
| `"auto"` | string | The default. Prefix a single-segment name when the upstream is Docker Hub                       |
| `true`   | bool   | Prefix a single-segment name whatever the upstream, for a Hub-compatible mirror on another host |
| `false`  | bool   | Never rewrite. Ask the upstream for the name the client typed                                   |

Any other value fails at startup: `` `library_prefix` must be true, false, or "auto" ``.

### What `auto` detects

`auto` reads the host of the index's `cached` URL and treats these three as Docker Hub:

- `docker.io`
- `index.docker.io`
- `registry-1.docker.io`

Any other host (`ghcr.io`, a Harbor, an Artifactory `/v2/` root, a self-hosted `distribution`) is not Hub, so `auto`
rewrites nothing there. The default costs a non-Hub proxy nothing.

### What is rewritten

Only a single-segment repository name: `ubuntu` becomes `library/ubuntu`, `nginx` becomes `library/nginx`.

### What is never rewritten

- A multi-segment name, under every value of the setting. `grafana/grafana` and `library/nginx` already name their
  namespace, and prefixing one would ask for a repository that does not exist.
- Any name on a non-Hub upstream under `auto`.
- Any name at all under `false`.

### Where the rewrite applies

The upstream request, and both halves of it:

- The request path: `GET /v2/library/ubuntu/manifests/24.04`.
- The bearer token scope peryx asks the upstream's token realm for: `repository:library/ubuntu:pull`. A token issued for
  the scope the client typed would not authorize the pull of the rewritten repository, so the two agree.

Everything on peryx's side keeps the spelling the client used:

- The local cache keys for manifests, blobs, and tags.
- The tag list (`/v2/hub/ubuntu/tags/list` names `hub/ubuntu`).
- The referrers index.
- The name the image is served, listed, and browsed under, in the API and the [web UI](@/core/web-ui.md).

`peryx mirror sync --index hub --image ubuntu:24.04` follows the same rule: it pulls `library/ubuntu` from Hub and
stores it as `ubuntu`.

## Related

- The task, with the `true` and `false` cases:
  [mirror Docker Hub official images](@/ecosystems/oci/guides/hub-official-images.md)
- Why Hub needs the namespace, and what an upstream `401` means:
  [Docker Hub names and upstream auth](@/ecosystems/oci/hub-names-and-auth.md)
- Every other TOML key: [configuration](@/core/configuration.md)
