+++
title = "Proxy a private mirror"
description = "Point velodex at Artifactory, GitLab, or any other PEP 503 index, with credentials."
weight = 3
+++

Declare a mirror index whose `mirror` key is the upstream's simple-index URL. Two authentication styles cover the
common servers; a bearer token wins when both are set.

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

## HTML-only upstreams

Some mirrors (Artifactory among them) serve only the PEP 503 HTML form. velodex asks for [PEP 691](https://peps.python.org/pep-0691/) JSON first, falls
back to parsing the HTML, and re-serves the result as JSON, so pip and uv get the modern format either way. You do
not configure this; it happens per response.

## Notes

- The config file holds these credentials, so restrict it: `chmod 600 velodex.toml`.
- Each mirror keeps its own credentials. A cached file remembers which mirror it came from, and a later cache-miss
  fetch reuses that mirror's authentication.
- `cache_ttl_secs` (default 1800) controls how long a cached project page is served before velodex revalidates it
  against the upstream with `If-None-Match`.


## Related

- Why one URL with shadowing beats `--extra-index-url`: [the index model](@/explanation/indexes.md)
- Serve a network with no internet route: [air-gapped](@/guides/air-gapped.md)
- Upstream capability differences velodex papers over: [standards](@/explanation/standards.md)
