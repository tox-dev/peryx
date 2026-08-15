# <img src="assets/icon.svg" width="28" alt=""> peryx

[![CI](https://github.com/tox-dev/peryx/actions/workflows/ci.yml/badge.svg)](https://github.com/tox-dev/peryx/actions/workflows/ci.yml)
[![Documentation](https://img.shields.io/readthedocs/peryx?logo=readthedocs&logoColor=white)](https://peryx.readthedocs.io/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://opensource.org/licenses/MIT)

Peryx is an async Rust artifact server. One executable links every shipped ecosystem owner and `peryx-ha-distributed`.
Each `[[index]]` selects an owner through `peryx-plugin-registry`; `[availability].mode` selects `none`, `dc`, or `ha`.
`none` skips availability assembly. Unselected owners install no runtime state or work.

## Properties

- Each index role shares a content-addressed blob store.
- Core crates define opaque IDs and focused traits. Owner crates implement those traits and keep protocol code and data
  inside their crate.
- `availability.mode = "none"` creates no distributed storage or runtime resource. It records no availability metrics;
  neutral request metrics remain enabled.
- `peryx-ha` defines availability contracts. `peryx-ha-distributed` owns datacenter and multi-datacenter resources and
  worker lifecycles.
- CI requires 100% line and function coverage from each crate and each declared system-test source root.

## Build and run

```shell
cargo build --release
./target/release/peryx serve
```

The process listens on `127.0.0.1:4433` by default. The
[configuration reference](https://peryx.readthedocs.io/en/latest/core/configuration/) documents index owners and
availability modes.

## Documentation

- [Architecture](https://peryx.readthedocs.io/en/latest/contributing/architecture/) defines crate and lifecycle
  ownership.
- [Ecosystem docs](https://peryx.readthedocs.io/en/latest/ecosystems/) contain client commands, protocol settings, and
  behavior.
- [Contributing](CONTRIBUTING.md) lists local and CI checks.

## License

The [MIT license](https://opensource.org/licenses/MIT) covers peryx.
