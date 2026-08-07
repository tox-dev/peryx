# <img src="assets/icon.svg" width="28" alt=""> peryx

[![CI](https://github.com/tox-dev/peryx/actions/workflows/ci.yml/badge.svg)](https://github.com/tox-dev/peryx/actions/workflows/ci.yml)
[![Documentation](https://img.shields.io/readthedocs/peryx?logo=readthedocs&logoColor=white)](https://peryx.readthedocs.io/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://opensource.org/licenses/MIT)

**Fast as the falcon, sealed as the pyx.** peryx is an async Rust artifact server for multiple packaging ecosystems.

Point `pip`, `uv`, or `twine` at it for Python and `docker` or any registry client at it for containers, all from one
process. Each index caches an upstream, hosts your own uploads, or merges the two so a package you publish shadows the
upstream of the same name. Ecosystem implementations plug into contracts owned by `peryx-core`.

## Highlights

- One binary contains the PyPI, OCI, and distributed availability implementations.
- `availability.mode = "none"` allocates no distributed state or work. The `dc` and `ha` modes select distributed
  behavior.
- Each ecosystem supports a caching proxy, a hosted store, and a virtual index that merges them.
- The content-addressed store keys artifacts by SHA-256. It stores identical bytes once across ecosystems and detects
  tampering.
- The server includes an allow/deny policy engine, full-text search, Prometheus metrics, and signed webhooks.
- `peryx-ecosystem-pypi` and `peryx-ecosystem-oci` own their protocol and policy behavior.

## Installation

Build from source and start the server:

```shell
cargo build --release
./target/release/peryx serve
```

peryx starts without configuration on `127.0.0.1:4433`. Use its [configuration](https://peryx.readthedocs.io/) to add
upstreams, hosted uploads, or cache settings.

## Documentation

[peryx.readthedocs.io](https://peryx.readthedocs.io/) contains tutorials, guides, reference pages, and design
explanations. Run `peryx --help` for the command-line reference.

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

- **Cached** proxies an upstream and keeps serving the last good copy for a bounded window when the upstream is
  unreachable.
- **Hosted** accepts and retains uploads.
- **Virtual** merges other indexes under one route, so a package you publish shadows the upstream of the same name.

### Built in

The server includes a neutral allow/deny [policy](https://peryx.readthedocs.io/) engine, full-text package search,
Prometheus-format metrics, and signed webhooks.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development setup and the
[architecture overview](https://peryx.readthedocs.io/en/latest/contributing/architecture/) for how the crates fit
together.

## License

The [MIT license](https://opensource.org/licenses/MIT) covers peryx.
