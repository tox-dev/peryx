+++
title = "Proxy a private upstream"
description = "Point peryx at Artifactory, GitLab, or any other PEP 503 index, with credentials."
weight = 3
+++

Declare a cached index whose `cached` field is the upstream's simple-index URL. Two authentication styles cover the
common servers; a bearer token wins when you set both.

## Artifactory or GitLab (bearer token)

```toml
[[index]]
name = "corp"
cached = "https://myco.jfrog.io/artifactory/api/pypi/pypi/simple/"
token = "<access-token>"
```

## pypi.org-style Basic auth

pypi.org tokens use the `__token__` username convention from the
[`.pypirc` specification](https://packaging.python.org/en/latest/specifications/pypirc/):

```toml
[[index]]
name = "corp"
cached = "https://private.example/simple/"
username = "__token__"
password = "<token>"
```

Start peryx with `--config` and install through `http://<host>:<port>/corp/simple/`.

## Sync for offline use

`peryx mirror sync` uses the same upstream URL and credentials as serving. Configure the working set next to the
cached index, then populate and verify the cache while the upstream is reachable:

```toml
[[index]]
name = "corp"
cached = "https://private.example/simple/"
token = "<token>"

[index.prefetch]
requirements = ["requirements.txt"]
```

```shell
peryx mirror sync corp --config peryx.toml
peryx mirror verify corp --config peryx.toml
peryx serve --config peryx.toml --offline
```

Set `offline = true` on the cached index when only that upstream should stay cache-only. Use the top-level
`offline = true` or `serve --offline` when every cached index in the process must avoid network access.

## HTML upstreams

Some upstreams, including Artifactory, serve the PEP 503 HTML form instead of PEP 691 JSON. peryx requests
[PEP 691](https://peps.python.org/pep-0691/) JSON first, parses HTML when the upstream returns it, and serves JSON to
pip and uv. You do not configure this; it happens per response. The upstream response must send a Simple API content
type (`text/html`, `application/vnd.pypi.simple.v1+html`, or `application/vnd.pypi.simple.v1+json`); other content types
return `502` with the upstream URL in the error body.

## Notes

- The config file holds these credentials, so restrict it: `chmod 600 peryx.toml`.
- Each cached index keeps its own credentials. A cached file remembers which cached index it came from, and a later
  cache-miss fetch reuses that index's authentication.
- Peryx asks upstream for `Accept-Encoding: identity` during artifact downloads. This makes the bytes pip and uv
  verify match the cached bytes. Same-host redirects keep the cached index's credentials.
- `cache_ttl_secs` (default 1800) controls how long peryx serves a cached project page before revalidating it against
  the upstream with `If-None-Match`.
- Peryx caches upstream `404` misses for project pages and `.metadata` siblings for 30 seconds.

## Related

- Why one URL with shadowing beats `--extra-index-url`: [the index model](@/core/indexes.md)
- Serve a network with no internet route: [air-gapped](@/ecosystems/pypi/guides/air-gapped.md)
- Upstream capability differences peryx papers over: [standards](@/ecosystems/pypi/reference/standards.md)
