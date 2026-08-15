+++
title = "Getting started"
description = "Install the binary, start a local process, and choose an ecosystem owner guide."
weight = 1
+++

## Install

Use the installer on Unix:

```shell
curl -LsSf https://github.com/tox-dev/peryx/releases/latest/download/peryx-installer.sh | sh
```

Or build from a checkout:

```shell
cargo build --release
```

See [installation](@/core/installation.md) for Windows and update behavior.

## Start the process

```shell
peryx serve
```

A source build can run `./target/release/peryx serve`. With no config file, the process listens on
`http://127.0.0.1:4433` and uses `availability.mode = "none"`. That mode starts no availability subsystem.

Check the shared status surface:

```shell
curl --fail http://127.0.0.1:4433/+status
```

## Configure an ecosystem

The binary includes the shipped ecosystem owners. Each `[[index]]` selects one with its `ecosystem` key; the
`[availability]` table selects `none`, `dc`, or `ha` for the process. The TOML file holds both choices.

Owner guides define valid IDs and owner-specific behavior:

- [Ecosystem owner documentation](@/ecosystems/_index.md)

Use the [configuration reference](@/core/configuration.md) for shared process, storage, access, job, and availability
keys. `peryx config check --config peryx.toml` rejects unknown owner IDs, unknown availability modes, and incomplete
distributed settings before startup.

## Default indexes

With no explicit `[[index]]`, configuration collects default indexes from all linked owner registrations. Adding any
explicit `[[index]]` replaces that complete default topology.
