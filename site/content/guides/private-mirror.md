+++
title = "Proxy a private mirror"
description = "Point velodex at Artifactory, GitLab, or any other PEP 503 index, with credentials."
weight = 3
+++

Declare a mirror index whose `mirror` field is the upstream's simple-index URL. Two authentication styles cover the
common servers; a bearer token wins when you set both.

## Artifactory or GitLab (bearer token)

```toml
[[index]]
name = "corp"
mirror = "https://myco.jfrog.io/artifactory/api/pypi/pypi/simple/"
token = "<access-token>"
```

## pypi.org-style Basic auth

pypi.org tokens use the `__token__` username convention from the
[`.pypirc` specification](https://packaging.python.org/en/latest/specifications/pypirc/):

```toml
[[index]]
name = "corp"
mirror = "https://private.example/simple/"
username = "__token__"
password = "<token>"
```

Start velodex with `--config` and install through `http://<host>:<port>/corp/simple/`.

## HTML upstreams

Some mirrors, including Artifactory, serve the PEP 503 HTML form instead of PEP 691 JSON. velodex requests
[PEP 691](https://peps.python.org/pep-0691/) JSON first, parses HTML when the upstream returns it, and serves JSON to
pip and uv. You do not configure this; it happens per response. The upstream response must send a Simple API content
type (`text/html`, `application/vnd.pypi.simple.v1+html`, or `application/vnd.pypi.simple.v1+json`); other content types
return `502` with the upstream URL in the error body.

## Notes

- The config file holds these credentials, so restrict it: `chmod 600 velodex.toml`.
- Each mirror keeps its own credentials. A cached file remembers which mirror it came from, and a later cache-miss fetch
  reuses that mirror's authentication.
- Velodex asks upstream for `Accept-Encoding: identity` during artifact downloads. This makes the bytes pip and uv
  verify match the cached bytes. Same-host redirects keep the mirror's credentials.
- `cache_ttl_secs` (default 1800) controls how long velodex serves a cached project page before revalidating it against
  the upstream with `If-None-Match`.
- Velodex caches upstream `404` misses for project pages and `.metadata` siblings for 30 seconds.

## Related

- Why one URL with shadowing beats `--extra-index-url`: [the index model](@/explanation/indexes.md)
- Serve a network with no internet route: [air-gapped](@/guides/air-gapped.md)
- Upstream capability differences velodex papers over: [standards](@/explanation/standards.md)
