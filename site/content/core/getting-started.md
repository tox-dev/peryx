+++
title = "Getting started"
description = "Install peryx, start it with no configuration, then continue with the ecosystem you serve: PyPI or OCI."
weight = 1
+++

This page gets a peryx binary running on your machine. It is the same first two steps whatever you serve (install the
binary, start the server), so it lives here in Core. Once peryx is listening, pick the ecosystem you use and its
getting-started tutorial carries on from there: caching an upstream, installing or pulling, and publishing your own.

## Prerequisites

Two things: a peryx binary, and a client for the ecosystem you serve: an installer like `pip` or `uv` for Python
packages, or a container client like `docker` or `podman` for images. The ecosystem tutorial names the exact client.

Install peryx through whichever channel fits; [installation](@/core/installation.md) lists them all:

{% tabs(names="installer, uv, pip, from source") %}

```shell
# standalone binary, no Python involved
curl -LsSf https://github.com/gaborbernat/peryx/releases/latest/download/peryx-installer.sh | sh
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
git clone https://github.com/gaborbernat/peryx.git
cd peryx
cargo build --release
```

{% end %}

## Start peryx

peryx needs no configuration to start. Run it and it listens on `127.0.0.1:4433` with a default topology: a cached
proxy of an upstream, a private hosted store, and a virtual index combining them:

```shell
peryx serve            # ./target/release/peryx serve when built from source
```

Open [http://127.0.0.1:4433/](http://127.0.0.1:4433/) for the web dashboard: the configured indexes, their
[roles](@/core/indexes.md), and live request counters. Leave the server running.

## Continue with your ecosystem

peryx is up. From here the steps depend on what you serve: the client, the wire protocol, and how you publish differ
by ecosystem. Follow the tutorial for yours:

- [**PyPI**: Python packages](@/ecosystems/pypi/tutorials/getting-started.md): cache pypi.org, install with pip and uv,
  publish a private package, then yank and delete it.
- [**OCI**: container images](@/ecosystems/oci/tutorials/getting-started.md): cache Docker Hub, pull an image, build and
  push one of your own, then verify it round-trips.

Each starts from a running peryx and takes about ten minutes.

## Where next

- [The index model](@/core/indexes.md): cached, hosted, and virtual indexes, and how a virtual index resolves.
- [Configuration reference](@/core/configuration.md): every TOML key.
- [Ecosystems](@/ecosystems/_index.md): the per-ecosystem "Set Me Up" hubs.
