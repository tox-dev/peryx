+++
title = "Yank a release by an equivalent version"
description = "Publish a release with form version 1.0, yank it by addressing 1.0.0, and watch the yank take effect even though the two spellings differ."
weight = 5
+++

In this tutorial you publish a release whose version is `1.0`, then yank it with a request addressed to `1.0.0`, and
watch the yank land even though the two spellings are not byte-identical. It takes about ten minutes and shows that
peryx addresses a release by [PEP 440](https://peps.python.org/pep-0440/) equality, the way pip and pypi.org do, not by
matching the exact string you typed.

## Prerequisites

You need a peryx binary ([installation](@/core/installation.md) lists the channels), Python with
[build](https://build.pypa.io/) and [twine](https://twine.readthedocs.io/), and a scratch directory.

## Start peryx with an upload token

Uploads are off until a hosted index has a token. Write a config that sets one and start peryx:

```toml
# peryx.toml
[[index]] # cached: read-through cache of pypi.org
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]] # hosted: your own uploads, gated by the token
name = "hosted"
upload_token = "demo-secret"

[[index]] # virtual: uploads shadow upstream behind one URL
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"
```

```shell
peryx serve --config peryx.toml
```

peryx listens on `127.0.0.1:4433`. Leave it running and use a second terminal.

## Build a release versioned 1.0

Create a minimal project whose version is exactly `1.0`:

```shell
mkdir demo && cd demo
```

```toml
# pyproject.toml
[build-system]
requires = ["setuptools>=61"]
build-backend = "setuptools.build_meta"

[project]
name = "demo-pkg"
version = "1.0"

[tool.setuptools]
py-modules = ["demo_pkg"]
```

```shell
touch demo_pkg.py
python -m build
```

The build writes `dist/demo_pkg-1.0.tar.gz` and `dist/demo_pkg-1.0-py3-none-any.whl`. The version on both filenames, and
in the metadata twine records, is `1.0`.

## Publish it

Upload to the virtual index's route. peryx accepts any username; the token is the password:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ \
    -u __token__ -p demo-secret dist/*
```

Confirm the release is live and not yet yanked:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/demo-pkg/ | python3 -m json.tool | grep -A2 filename
```

The files list `"yanked": false`.

## Yank it, addressing 1.0.0

Now yank the release, but address it as `1.0.0` rather than the `1.0` it was published with:

```shell
curl -X PUT -u __token__:demo-secret \
    http://127.0.0.1:4433/root/pypi/demo-pkg/1.0.0/yank
```

peryx answers `200` with a non-zero count of files changed. `1.0.0` is the same release as `1.0` under PEP 440, so the
request reached both files. Before peryx compared versions this way, the same request matched nothing, returned zero,
and left the release live.

## Confirm the yank took

Read the project page again:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/demo-pkg/ | python3 -m json.tool | grep yanked
```

Both files now report `"yanked": true`. A resolver skips them, while a build pinned to the exact version can still fetch
them, exactly as [PEP 592](https://peps.python.org/pep-0592/) prescribes.

## What you saw

You published a release as `1.0` and yanked it by addressing `1.0.0`, and the yank reached every file of the release.
peryx matches a version-scoped operation to a release by PEP 440 equality, so the spelling you type does not have to be
the spelling the file was uploaded with. A request for `1.0.0` would still never touch `1.0.1`; equality is one release,
not a range.

## Where next

- Do this for yank, delete, and promote against your own releases:
  [target a release by version](@/ecosystems/pypi/guides/version-match.md)
- The exact matching rule and its examples:
  [version matching for admin operations](@/ecosystems/pypi/reference/version-match.md)
- Why peryx matches this way: [equivalent version spellings](@/ecosystems/pypi/version-match.md)
