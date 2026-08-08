+++
title = "PyPI"
description = "How PyPI maps to cached, hosted, and virtual indexes, the Simple API, and pip, uv, and twine configuration."
weight = 1
sort_by = "weight"
template = "section.html"
[extra]
logos = [ "logos/pypi.svg"]
+++

PyPI defines the wheel and sdist formats and the HTTP protocol that installers use to find and download them. A
**[wheel](https://packaging.python.org/en/latest/specifications/binary-distribution-format/)** (`.whl`) is a built
package that an installer can install; an
**[sdist](https://packaging.python.org/en/latest/specifications/source-distribution-format/)** (source distribution, a
`.tar.gz` or a `.zip`) contains the source used to build a wheel. Both are **artifacts**, the files an installer
fetches.

## How PyPI concepts map to peryx

peryx uses one vocabulary across ecosystems. Its Python terms include Python's own `index` and `project` names. See
[the index model](@/core/indexes.md) and [glossary](@/core/glossary.md).

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

peryx uses the role names **cached**, **hosted**, and **virtual**, plus **shadowing**, across ecosystems.

## The roles for PyPI

The three [index roles](@/core/indexes.md) map to PyPI as follows:

- **cached**: a read-through cache of an upstream Python index such as [pypi.org](https://pypi.org/). On a miss, peryx
  fetches, stores, and serves the project page or artifact. Later requests use the stored copy. A cached index can front
  pypi.org, [TestPyPI](https://test.pypi.org/), [Artifactory](https://jfrog.com/artifactory/), or a GitLab registry.
- **hosted**: a store for wheels and sdists published through the standard upload API. Twine or `uv publish` writes the
  files without using an upstream.
- **virtual**: an ordered stack of cached and hosted indexes under one URL. Hosted uploads shadow upstream files with
  the same name. Clients use one `index-url` without `--extra-index-url`.

A cached route retries upstream server errors, timeouts, and `429` responses with bounded backoff. A valid `Retry-After`
delay or HTTP date takes precedence, capped at 30 seconds.

## The wire protocol

Python installers speak the
**[Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/)**. An index exposes one
page per project with links to its files. peryx supports these forms:

- **[PEP 503](https://peps.python.org/pep-0503/)**: the original HTML page of download links. peryx parses it from
  upstreams that only speak HTML.
- **[PEP 691](https://peps.python.org/pep-0691/)**: the modern JSON form of the same data. peryx canonicalizes every
  upstream to this once, at fetch time, and serves JSON (with HTML on request) downstream.
- **[PEP 658/714](https://peps.python.org/pep-0658/)**: a `.metadata` sibling next to each file lets a resolver read a
  few kilobytes of dependency metadata without downloading a wheel. peryx serves it and synthesizes it with byte-range
  reads when an upstream lacks it.
- **[Legacy upload API](https://docs.pypi.org/api/upload/)**: the POST endpoint twine and `uv publish` use to publish
  into a hosted index.

For the full standards map, see [standards](@/ecosystems/pypi/reference/standards.md).

## Configure clients

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

### Generate configuration files

`peryx config-snippet` prints `pip.conf`, `uv.toml`, or `.pypirc` without starting the server:

```shell
peryx config-snippet --base-url https://packages.example --index root/pypi pip.conf
peryx config-snippet --base-url https://packages.example --index root/pypi uv.toml
peryx config-snippet --base-url https://packages.example --index root/pypi .pypirc
```

`--base-url` is the public origin, including any proxy path prefix and excluding the index route. `pip.conf` and
`uv.toml` work for read-only and writable indexes. `.pypirc` requires a hosted upload target that accepts writes, and
the generated file contains `<upload-token>` instead of the configured secret.

PyPI metadata pages use the transformed-page memory cache controlled by the top-level `hot_cache_bytes` setting. Setting
it to `0` disables that cache without removing stored pages or distributions.

## Web UI

The PyPI extension labels searchable entities as packages and opens an index card on its project list. A project page
shows the long description, summary, install command, versions, dependencies, project links, classifiers, and files.
Release groups follow PEP 440 order. Files that do not map to one declared release remain visible under **Legacy or
unassociated files**.

Each file row shows size, upload time, sha256, yank state, metadata availability, source, and byte availability. A file
with PEP 740 provenance has a disclosure for its predicate types and subject binding. The disclosure reports claims and
binding checks; it does not claim that peryx verified a Sigstore signature, certificate, or transparency log.

Wheels, zip files, zipped eggs, `.tar`, `.tar.gz`, and `.tgz` files expose archive contents. The browser lists members
and previews bounded text chunks. Other compressed tar formats remain download-only.

An upload-enabled route adds an **Upload** page for one wheel or `.tar.gz` source distribution. A project page adds
**Manage uploads** actions for yank, un-yank, delete, and restore. These controls call the same endpoints documented in
[yank and delete packages](@/ecosystems/pypi/guides/remove.md).

{{ screen(alt="A project page with description, metadata, releases, and distribution files", name="project") }}

## Related

- How peryx compares to devpi, proxpi, pypiserver, and pypicloud: [PyPI performance](@/ecosystems/pypi/performance.md)
- Front an index that is not pypi.org: [front another index](@/ecosystems/pypi/tutorials/front-another-index.md)
- Add credentials for a private upstream: [proxy a private upstream](@/ecosystems/pypi/guides/private-mirror.md)
- Publish your own packages: [publish](@/ecosystems/pypi/guides/publish.md)
- Block compromised distributions without deleting evidence: [digest revocations](@/ecosystems/pypi/revoked-content.md)
