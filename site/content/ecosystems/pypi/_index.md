+++
title = "PyPI"
description = "The Python ecosystem: what cached, hosted, and virtual mean for PyPI, the Simple API wire protocol, and pip/uv/twine config."
weight = 1
sort_by = "weight"
template = "section.html"
[extra]
logos = [ "logos/pypi.svg"]
+++

PyPI is the Python packaging ecosystem: the format of wheels and sdists, and the HTTP protocol installers use to find
and download them. A **[wheel](https://packaging.python.org/en/latest/specifications/binary-distribution-format/)**
(`.whl`) is a pre-built, ready-to-install package; an
**[sdist](https://packaging.python.org/en/latest/specifications/source-distribution-format/)** (source distribution, a
`.tar.gz` or a `.zip`) is the source a wheel is built from. Both are **artifacts**: the actual files an installer
fetches.

## How PyPI concepts map to peryx

peryx describes every ecosystem with one neutral vocabulary; for Python it mostly matches the terms you already use,
since peryx borrows Python's own words (index, project). The neutral name is what the same idea is called across
ecosystems (see [the index model](@/core/indexes.md) and [glossary](@/core/glossary.md)).

| Python term                | peryx concept    | What it is                                                                                             |
| -------------------------- | ---------------- | ------------------------------------------------------------------------------------------------------ |
| index (`--index-url`)      | index            | the endpoint a client points at; a cached index proxies one upstream                                   |
| project / package          | project          | one distribution name, like `requests`                                                                 |
| release / version          | version          | one released version of a project                                                                      |
| distribution (wheel/sdist) | artifact         | what you install: a `.whl`, `.tar.gz`, or `.zip` file                                                  |
| file                       | file             | one content-addressed distribution file                                                                |
| publish / upload           | upload / publish | putting a distribution into a hosted index with [twine](https://twine.readthedocs.io/) or `uv publish` |
| install / download         | download         | fetching a distribution through peryx                                                                  |
| pull-through mirror        | cached (role)    | a read-through proxy of one upstream index                                                             |

The role names (**cached**, **hosted**, **virtual**) and **shadowing** are peryx's own, the same in every ecosystem.

## The roles for PyPI

The three [index roles](@/core/indexes.md) map onto PyPI like this:

- **cached**: a read-through cache of an upstream Python index such as [pypi.org](https://pypi.org/). On a miss peryx
  fetches the project page or artifact from upstream, stores it, and serves it; later requests come from disk. Point one
  at pypi.org, a [TestPyPI](https://test.pypi.org/), an [Artifactory](https://jfrog.com/artifactory/), or a GitLab
  registry.
- **hosted**: a store you publish your own wheels and sdists to over the standard upload API. Nothing upstream; the
  files live here because twine or `uv publish` put them there.
- **virtual**: an ordered stack of the two, served under one URL, where your hosted uploads shadow same-named upstream
  files. This is what clients point at: one `index-url`, private packages winning over public ones, no
  `--extra-index-url`.

## The wire protocol

Python installers speak the
**[Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/)**: an index exposes a page
per project listing that project's files, and the installer downloads what it resolves. peryx serves and understands
every current form:

- **[PEP 503](https://peps.python.org/pep-0503/)**: the original HTML page of download links. peryx parses it from
  upstreams that only speak HTML.
- **[PEP 691](https://peps.python.org/pep-0691/)**: the modern JSON form of the same data. peryx canonicalizes every
  upstream to this once, at fetch time, and serves JSON (with HTML on request) downstream.
- **[PEP 658/714](https://peps.python.org/pep-0658/)**: a `.metadata` sibling next to each file, so a resolver reads a
  few kilobytes of dependency metadata instead of downloading a whole wheel. peryx serves it, and synthesizes it with
  byte-range reads when an upstream lacks it.
- **[Legacy upload API](https://docs.pypi.org/api/upload/)**: the POST endpoint twine and `uv publish` use to publish
  into a hosted index.

For the full standards map, see [standards](@/ecosystems/pypi/reference/standards.md).

## Set me up

Assume peryx is running at `http://127.0.0.1:4433` with the default virtual route `root/pypi`. Installers read from
`.../simple/`; publishers post to the route root.

### Install

{% tabs(names="pip, uv, poetry") %}

```shell
# one-off
pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests

# persistent: environment
export PIP_INDEX_URL=http://127.0.0.1:4433/root/pypi/simple/

# persistent: pip.conf (~/.config/pip/pip.conf or venv pip.conf)
# [global]
# index-url = http://127.0.0.1:4433/root/pypi/simple/
```

%%%

```shell
# one-off
uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests

# persistent: environment
export UV_INDEX_URL=http://127.0.0.1:4433/root/pypi/simple/
```

%%%

```shell
poetry source add --priority=primary peryx http://127.0.0.1:4433/root/pypi/simple/
```

{% end %}

### Publish

Publishing can use a [hosted layer with an upload token](@/ecosystems/pypi/guides/publish.md) or exchange a
[GitHub Actions or GitLab CI identity](@/ecosystems/pypi/guides/trusted-publishing.md) for a short-lived token. The CI
flow scopes that token to one repository route and the configured project globs.

{% tabs(names="twine, uv, .pypirc") %}

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <token> dist/*
```

%%%

```shell
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p <token> dist/*
```

%%%

```ini
# ~/.pypirc
[distutils]
index-servers = peryx

[peryx]
repository = http://127.0.0.1:4433/root/pypi/
username = __token__
password = <token>
```

{% end %}

`GET /root/pypi/+api` returns a ready-made `.pypirc` snippet for any configured route.

## In practice

- How peryx compares to devpi, proxpi, pypiserver, and pypicloud: [PyPI performance](@/ecosystems/pypi/performance.md)
- Front an index that is not pypi.org: [front another index](@/ecosystems/pypi/tutorials/front-another-index.md)
- Add credentials for a private upstream: [proxy a private upstream](@/ecosystems/pypi/guides/private-mirror.md)
- Publish your own packages: [publish](@/ecosystems/pypi/guides/publish.md)
