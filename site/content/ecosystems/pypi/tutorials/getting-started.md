+++
title = "Getting started"
description = "Serve Python packages through peryx: cache pypi.org, install with pip and uv, publish a private package, then yank and delete it."
weight = 1
+++

In this tutorial you point peryx at pypi.org, install packages through it with [pip](https://pip.pypa.io/) and uv,
publish a package of your own to a private hosted store, then yank and delete it. It takes about ten minutes.

A Python package ships as one or both of two **artifacts**: a
**[wheel](https://packaging.python.org/en/latest/specifications/binary-distribution-format/)** (`.whl`), a pre-built
archive an installer unpacks as-is, and an
**[sdist](https://packaging.python.org/en/latest/specifications/source-distribution-format/)** (`.tar.gz`), the source a
wheel is built from. Installers find them through the
[Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/), the HTTP protocol peryx
speaks to `pip`, `uv`, and `twine`.

## Prerequisites

You need Python with `pip` or [`uv`](https://docs.astral.sh/uv/) as the client, and a peryx binary. Pick whichever
install channel fits; [installation](@/core/installation.md) lists them all:

{% tabs(names="installer, uv, pip, from source") %}

```shell
# standalone binary, no Python involved
curl -LsSf https://github.com/tox-dev/peryx/releases/latest/download/peryx-installer.sh | sh
```

%%%

```shell
uv tool install peryx
```

%%%

```shell
pip install peryx
```

%%%

```shell
# needs a Rust toolchain (https://rustup.rs); rust-toolchain.toml pins the version
git clone https://github.com/tox-dev/peryx.git
cd peryx
cargo build --release
```

{% end %}

## Start peryx

The read path needs no configuration. The defaults give you a pypi.org cached index with a private hosted store,
combined by a virtual index in front of them, served at route `root/pypi`:

```shell
peryx serve            # ./target/release/peryx serve when built from source
```

peryx is now listening on `127.0.0.1:4433`. Leave it running and use a second terminal for the rest of the tutorial.

## Install through the cache

Point any installer at the index URL. The first install fetches from pypi.org, verifies each artifact against its
sha256, and caches it; repeat installs come from disk:

{% tabs(names="uv, pip") %}

```shell
uv venv demo
VIRTUAL_ENV=demo uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests
```

%%%

```shell
python -m venv demo
demo/bin/pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests
```

{% end %}

Both clients use the [PEP 658](https://peps.python.org/pep-0658/) metadata fast path through peryx: they resolve
dependency trees by fetching small `.metadata` files rather than whole wheels. You can watch it on the metrics endpoint:

```shell
curl -s http://127.0.0.1:4433/metrics | grep metadata
```

## Publish a private package

Uploads are disabled until you set a token. Stop peryx, write a config that adds one, and restart:

```toml
# peryx.toml
[[index]] # cached: read-through cache of pypi.org
name = "pypi"
cached = "https://pypi.org/simple/"

[[index]] # hosted: your own packages, upload needs the token
name = "hosted"
upload_token = "demo-secret"

[[index]] # virtual: your uploads shadow upstream behind one URL
name = "root/pypi"
layers = ["hosted", "pypi"]
upload = "hosted"
```

```shell
peryx serve --config peryx.toml
```

Now publish a wheel with [twine](https://twine.readthedocs.io/) or uv (any username; the token is the password):

```shell
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p demo-secret dist/*
```

Your package installs from the same URL as everything else: the virtual index serves your upload and pypi.org side by
side, and your file shadows any upstream file with the same name.

## Yank and delete it

Mark a version yanked ([PEP 592](https://peps.python.org/pep-0592/)) so resolvers skip it while pinned installs still
work, then delete it outright:

```shell
curl -X PUT    -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/mypkg/1.0.0/yank
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/mypkg/
```

The same actions live in the web UI: open the project page, expand "Manage uploads", and enter the token. After the
delete, the upstream version of `mypkg` (if pypi.org has one) is visible again.

## Verify

peryx serves a web interface on the same port. Open [http://127.0.0.1:4433/](http://127.0.0.1:4433/) for a live
dashboard of the configured indexes and request counters, click an index for a searchable project list, and click a
project for a pypi.org-style page: description, dependencies, classifiers, and files with hashes. The same counters are
JSON at `/+stats` and [Prometheus](https://prometheus.io/) at `/metrics`.

## Where next

- [Front another index](@/ecosystems/pypi/tutorials/front-another-index.md): cache a private index alongside pypi.org.
- [Build a team index](@/ecosystems/pypi/tutorials/team-index.md): a shared upload store your whole team installs from.
- [PyPI performance](@/ecosystems/pypi/performance.md): how peryx compares to devpi, proxpi, and pip's own cache.
- [Configuration reference](@/core/configuration.md): every TOML key, including TLS.
