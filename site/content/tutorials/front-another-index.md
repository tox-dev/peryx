+++
title = "Front another index"
description = "Mirror an index that is not pypi.org, watch velodex upgrade its protocols, and layer it with PyPI under one URL."
weight = 3
+++

In this tutorial you will point velodex at a second upstream, TestPyPI, and serve it two ways: on its own route, and
layered with pypi.org under one URL. The same recipe fronts an Artifactory, a GitLab registry, or any other PEP 503
index. It takes about ten minutes.

## Why TestPyPI

[TestPyPI](https://test.pypi.org/) is the packaging ecosystem's sandbox: public, credential-free for reads, and separate
from the real index, which makes it a safe stand-in for "some other index you must serve". It also lacks some of
pypi.org's polish, which is the point: you will watch velodex paper over the difference.

## Mirror it

```toml
# velodex.toml
data_dir = "velodex-data"

[[index]]
name = "testpypi"
mirror = "https://test.pypi.org/simple/"
```

```shell
velodex serve --config velodex.toml
uv venv demo
VIRTUAL_ENV=demo uv pip install --index-url http://127.0.0.1:4433/testpypi/simple/ sampleproject
```

`sampleproject` is TestPyPI's demonstration package. It installed through your mirror, was verified against the hashes
its index page advertised, and is now cached: rerun the install with a fresh environment and it comes from disk.

## See the protocol upgrade

Ask your mirror for the page and note what you get back:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/testpypi/simple/sampleproject/ | python3 -m json.tool | head
```

A PEP 691 JSON document with PEP 700 fields, whatever the upstream offered. velodex negotiates the best format the
upstream has, canonicalizes it once, and serves the modern stack downstream; an HTML-only Artifactory gets the same
treatment ([how the degradation works](@/explanation/standards.md)).

## Layer it with PyPI

One route that prefers the private-ish index and falls back to the big one:

```toml
[[index]]
name = "pypi"
mirror = "https://pypi.org/simple/"

[[index]]
name = "both"
layers = ["testpypi", "pypi"]
```

Restart, then install something that only exists on pypi.org through the combined route:

```shell
VIRTUAL_ENV=demo uv pip install --index-url http://127.0.0.1:4433/both/simple/ requests
```

Resolution walked the layers in order: TestPyPI first (miss), pypi.org second (hit). One `index-url`, both sources,
first match wins per file. With a real private upstream you would add credentials to its `[[index]]` entry
([the guide](@/guides/private-mirror.md) lists the URL and auth shape for each provider).

## Where next

- The provider-by-provider URL and credential reference: [proxy a private mirror](@/guides/private-mirror.md)
- What layering means for security: [the index model](@/explanation/indexes.md)
- Coming from one of these providers? [Migration](@/migration/_index.md)
