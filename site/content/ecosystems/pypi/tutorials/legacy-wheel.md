+++
title = "Upload a legacy wheel"
description = "Publish a real historical wheel whose .dist-info directory predates PEP 503 normalization, and watch peryx accept and serve it."
weight = 4
+++

In this tutorial you publish a wheel that PyPI has carried for years, one whose internal `.dist-info` directory is
spelled the un-normalized way older build tools wrote it, and watch peryx accept it and serve it back. It takes about
ten minutes and shows that peryx takes the same wheels the index it fronts does.

## Prerequisites

You need a peryx binary ([installation](@/core/installation.md) lists the channels), Python with
[pip](https://pip.pypa.io/), and [twine](https://twine.readthedocs.io/) to upload. Work in a scratch directory.

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

## Fetch a historical wheel

[Flask 0.12](https://pypi.org/project/Flask/0.12/) shipped in 2016, before the ecosystem settled on normalized
`.dist-info` names. Download its wheel from pypi.org:

```shell
pip download Flask==0.12 --no-deps --only-binary :all: --dest dist
```

Look inside it and note the directory name:

```shell
unzip -l dist/Flask-0.12-py2.py3-none-any.whl | grep dist-info
```

The directory is `Flask-0.12.dist-info`, mixed case. The filename normalizes to `flask`, so the directory name and the
normalized filename are not byte-for-byte equal. pip installs this wheel every day; the question is whether peryx will
take it on upload.

## Publish it

Upload to the virtual index's route. peryx accepts any username; the token is the password:

```shell
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ \
    -u __token__ -p demo-secret dist/Flask-0.12-py2.py3-none-any.whl
```

twine reports the upload succeeded. peryx matched `Flask-0.12.dist-info` to the `flask-0.12` filename by normalizing the
name and parsing the version, rather than demanding the exact bytes, so the wheel passed validation. Before the change
that made peryx accept these, this same upload returned a `400`.

## Confirm it is served

Ask the index for the project page and find your file:

```shell
curl -s -H "Accept: application/vnd.pypi.simple.v1+json" \
    http://127.0.0.1:4433/root/pypi/simple/flask/ | python3 -m json.tool | grep -A1 Flask-0.12
```

Install it back through peryx into a fresh environment to prove the round trip:

```shell
python -m venv check
check/bin/pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ Flask==0.12
```

The wheel you published, un-normalized `.dist-info` and all, installed straight back out of peryx.

## What you saw

peryx compares a wheel's `.dist-info` directory to its filename by normalized project name and parsed version, not by
exact string, so it accepts the historical wheels that live on PyPI today. A directory that named a different project or
version would still be rejected. peryx accepts a different spelling of the right identity, never the wrong one.

## Where next

- Do this for your own back catalogue: [publish from older tooling](@/ecosystems/pypi/guides/legacy-wheel.md)
- The exact matching rule: [wheel .dist-info matching](@/ecosystems/pypi/reference/dist-info.md)
- Why peryx works this way: [un-normalized wheels](@/ecosystems/pypi/dist-info.md)
