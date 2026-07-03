+++
title = "Getting started"
description = "Install velodex, serve your first packages through it, publish a private package, and remove it again."
weight = 1
+++

In this tutorial you will install velodex, start it with no configuration, install packages through it with
pip and uv, publish a package of your own, then yank and delete it. It takes about ten minutes.

## Prerequisites

You need Python with `pip` or [`uv`](https://docs.astral.sh/uv/) to act as the client, and a velodex binary. Pick
whichever install channel fits; [installation](@/reference/installation.md) lists them all:

{% tabs(names="installer, uv, pip, from source") %}

```shell
# standalone binary, no Python involved
curl -LsSf https://github.com/tox-dev/velodex/releases/latest/download/velodex-installer.sh | sh
```

%%%

```shell
uv tool install velodex
```

%%%

```shell
pip install velodex
```

%%%

```shell
# needs a Rust toolchain (https://rustup.rs); rust-toolchain.toml pins the version
git clone https://github.com/tox-dev/velodex.git
cd velodex
cargo build --release
```

{% end %}

## Start velodex

Start the server. It needs no configuration; the defaults give you a pypi.org mirror with a private local store
overlaid in front of it, served at `root/pypi`:

```shell
velodex serve            # ./target/release/velodex serve when built from source
```

velodex is now listening on `127.0.0.1:4433`. Leave it running and use a second terminal for the rest of the tutorial.

## Install through the cache

Point any installer at the index URL. The first install fetches from pypi.org, verifies each artifact against its
sha256, and caches it; repeat installs come from disk.

```shell
uv venv demo
VIRTUAL_ENV=demo uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests
```

or with pip:

```shell
python -m venv demo
demo/bin/pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests
```

Both clients use the [PEP 658](https://peps.python.org/pep-0658/) metadata fast path through velodex: they resolve dependency trees by fetching small
`.metadata` files rather than whole wheels. You can see this on the metrics endpoint:

```shell
curl -s http://127.0.0.1:4433/metrics | grep metadata
```

## Browse the web UI

velodex serves its own web interface on the same port. Open [http://127.0.0.1:4433/](http://127.0.0.1:4433/) for a live
dashboard of the configured indexes and request counters, click an index to get a searchable project list, and click a
project for a pypi.org-style page: description, dependencies, classifiers, files with hashes, and a browser for the
contents of each supported archive. The `contents` links are shareable: the URL carries the file's sha256, display
filename, member path, and chunk offset separately, so filenames with spaces or URL punctuation still open the right
archive.

## Publish a private package

Uploads are disabled until you set a token. Stop velodex, write a minimal config that adds one, and restart:

```toml
# velodex.toml
[[index]]
name = "pypi"
mirror = "https://pypi.org/simple/"

[[index]]
name = "local"
upload_token = "demo-secret"

[[index]]
name = "root/pypi"
layers = ["local", "pypi"]
upload = "local"
```

```shell
./target/release/velodex serve --config velodex.toml
```

Now publish a wheel with [twine](https://twine.readthedocs.io/) or uv (any username; the token is the password):

```shell
uv publish --publish-url http://127.0.0.1:4433/root/pypi/ -u __token__ -p demo-secret dist/*
```

Your package installs from the same URL as everything else: the overlay serves your upload and pypi.org side by
side, and your file shadows any upstream file with the same name.

## Yank and delete it

Mark a version yanked ([PEP 592](https://peps.python.org/pep-0592/)) so resolvers skip it while pinned installs still
work, then delete it outright:

```shell
curl -X PUT    -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/mypkg/1.0.0/yank
curl -X DELETE -u __token__:demo-secret http://127.0.0.1:4433/root/pypi/mypkg/
```

The same actions live in the web UI: open the project page, expand "Manage uploads", and enter the token.

After the delete, the upstream version of `mypkg` (if one exists on pypi.org) is visible again.

## See what it served

The dashboard's index cards count pages, downloads, and bytes as you use them; the `usage` link drills down to
per-project and per-file numbers. The same counters are JSON at `/+stats` and Prometheus at `/metrics`; see
[monitoring](@/guides/monitor.md).

## Where next

- [How-to guides](@/guides/_index.md) for specific tasks like proxying an Artifactory mirror or composing overlays.
- [Configuration reference](@/reference/configuration.md) for every TOML key.
- [The index model](@/explanation/indexes.md) to understand mirrors, locals, and overlays in depth.
