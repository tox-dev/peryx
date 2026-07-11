# <img src="assets/icon.svg" width="28" alt=""> peryx

[![CI](https://github.com/tox-dev/peryx/actions/workflows/ci.yml/badge.svg)](https://github.com/tox-dev/peryx/actions/workflows/ci.yml)
[![Documentation](https://img.shields.io/readthedocs/peryx?logo=readthedocs&logoColor=white)](https://peryx.readthedocs.io/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://opensource.org/licenses/MIT)

**Fast as the falcon, sealed as the pyx.** peryx is one artifact server for many packaging ecosystems, written in async
Rust.

Point `pip`, `uv`, or `twine` at it for Python and `docker` or any registry client at it for containers, all from one
process. Each index caches an upstream, hosts your own uploads, or merges the two so a package you publish shadows the
upstream of the same name. A new ecosystem is a driver, not a rewrite.

## Highlights

- 🦅 One server for many ecosystems: PyPI and OCI today, with more added as drivers rather than forks.
- ⚡ One async Rust process: zero-config on a laptop, scaling to a cluster when configured.
- 🔀 Three roles for every ecosystem: a caching proxy, a hosted store you publish to, and a virtual index that merges
  them.
- 🔒 Content-addressed storage: each artifact is keyed by its SHA-256, so identical bytes are stored once, deduplicated
  across ecosystems, and tamper-evident.
- 🛡️ Batteries included: an allow/deny policy engine, full-text search, Prometheus metrics, and signed webhooks.
- 🧩 Neutral by design: the server names no format, and each ecosystem plugs into one interface.

## Installation

Build from source and start the server:

```shell
cargo build --release
./target/release/peryx serve
```

peryx runs zero-config on `127.0.0.1:4433`. Point it at real upstreams, turn on hosted uploads, and tune caching through
its [configuration](https://peryx.readthedocs.io/).

## Documentation

peryx's documentation lives at [peryx.readthedocs.io](https://peryx.readthedocs.io/): tutorials, how-to guides, the
configuration and endpoint reference, and design explanations. Run `peryx --help` for the command-line reference.

## Features

### Python (PyPI)

Serve the [Simple repository API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) as a
caching proxy, a hosted index, or a virtual blend of both:

```shell
uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ requests
twine upload --repository-url http://127.0.0.1:4433/root/pypi/ dist/*
```

### Containers (OCI)

Serve the [OCI distribution spec](https://github.com/opencontainers/distribution-spec) so any container client pulls and
pushes through peryx:

```shell
docker pull 127.0.0.1:4433/dockerhub/library/alpine
```

### Three roles per index

Every ecosystem gets the same three behaviors for free:

- **Cached** proxies an upstream and keeps serving the last good copy for a bounded window when the upstream is
  unreachable.
- **Hosted** accepts your uploads and holds them durably.
- **Virtual** merges other indexes under one route, so a package you publish shadows the upstream of the same name.

### Built in

A neutral allow/deny [policy](https://peryx.readthedocs.io/) engine, full-text package search, Prometheus-format
metrics, and signed webhooks ship with the server, not as add-ons.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development setup and the
[architecture overview](https://peryx.readthedocs.io/en/latest/contributing/architecture/) for how the crates fit
together. [proposal.md](proposal.md) holds the original design document and roadmap.

## License

peryx is licensed under the [MIT license](https://opensource.org/licenses/MIT).
