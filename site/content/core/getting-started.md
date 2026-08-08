+++
title = "Getting started"
description = "Install peryx, start its default server, and continue with an ecosystem tutorial."
weight = 1
+++

Install the peryx binary and start the server before configuring an ecosystem client. The ecosystem tutorials cover
client URLs, protocol operations, and publishing.

## Install peryx

[Installation](@/core/installation.md) describes each supported channel.

{% tabs(names="installer, uv, pip, from source") %}

```shell
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
git clone https://github.com/tox-dev/peryx.git
cd peryx
cargo build --release
```

{% end %}

The source install requires the Rust toolchain pinned by `rust-toolchain.toml`.

## Start the server

The default configuration listens on `127.0.0.1:4433` and creates cached, hosted, and virtual repository roles.

```shell
peryx serve
```

Use `./target/release/peryx serve` after a source build.

Open [http://127.0.0.1:4433/](http://127.0.0.1:4433/) to inspect configured repositories and request counters. Keep the
process running while following an ecosystem tutorial.

## Configure an ecosystem

- [Python package tutorial](@/ecosystems/pypi/tutorials/getting-started.md)
- [OCI tutorial](@/ecosystems/oci/tutorials/getting-started.md)

These tutorials define the client, repository URL, publication flow, and protocol checks for their ecosystem.

## Next steps

- [Repository roles](@/core/indexes.md)
- [Configuration](@/core/configuration.md)
- [Ecosystem documentation](@/ecosystems/_index.md)
