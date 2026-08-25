+++
title = "Front another index"
description = "Mirror a non-PyPI index, normalize its protocol, and layer it with PyPI under one URL."
weight = 3
+++

Point peryx at TestPyPI and serve it on its own route and layered with pypi.org under one URL. The same recipe fronts an
[Artifactory](https://jfrog.com/artifactory/), a GitLab registry, or any other
[PEP 503](https://peps.python.org/pep-0503/) index. It takes about ten minutes.

## TestPyPI upstream

[TestPyPI](https://test.pypi.org/) is the packaging ecosystem's public sandbox and requires no credentials for reads.
Its protocol differences exercise peryx's upstream normalization.

## Mirror it

```toml
# peryx.toml
data_dir = "peryx-data"

[[index]]
ecosystem = "pypi"
name = "testpypi"

[[index.upstream]]
name = "primary"
url = "https://test.pypi.org/simple/"
```

```shell
peryx serve --config peryx.toml
uv venv demo
VIRTUAL_ENV=demo uv pip install --index-url http://127.0.0.1:4433/testpypi/simple/ sampleproject
```

`sampleproject` is TestPyPI's demonstration package. It installed through your cached index, was verified against the
hashes its index page advertised, and is now cached: rerun the install with a fresh environment and it comes from disk.

## See the protocol upgrade

Ask your cached index for the page and note what you get back:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/testpypi/simple/sampleproject/ | python3 -m json.tool | head
```

A [PEP 691](https://peps.python.org/pep-0691/) JSON document with [PEP 700](https://peps.python.org/pep-0700/) fields,
whatever the upstream offered. peryx negotiates the best format the upstream has, canonicalizes it once, and serves the
modern stack downstream; an HTML-only Artifactory gets the same treatment
([how the degradation works](@/ecosystems/pypi/reference/standards.md)).

## Layer it with PyPI

One route that prefers the private-ish index and falls back to the big one:

```toml
[[index]]
ecosystem = "pypi"
name = "pypi"

[[index.upstream]]
name = "primary"
url = "https://pypi.org/simple/"

[[index]]
ecosystem = "pypi"
name = "both"
layers = ["testpypi", "pypi"]
```

Restart, then install something that only exists on pypi.org through the combined route:

```shell
VIRTUAL_ENV=demo uv pip install --index-url http://127.0.0.1:4433/both/simple/ requests
```

Resolution walked the layers in order: TestPyPI first (miss), pypi.org second (hit). One `index-url`, both sources,
first match wins per file. With a real private upstream you would add credentials to its `[[index]]` entry
([the guide](@/ecosystems/pypi/guides/private-mirror.md) lists the URL and auth shape for each provider).

## Next steps

- The provider-by-provider URL and credential reference:
  [proxy a private upstream](@/ecosystems/pypi/guides/private-mirror.md)
- What layering means for security: [the index model](@/core/repositories/indexes.md)
- Coming from one of these providers? [Migration](@/ecosystems/pypi/migration/_index.md)
