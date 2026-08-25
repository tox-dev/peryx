# <img src="site/static/icon.svg" width="28" alt=""> peryx

[![CI](https://github.com/tox-dev/peryx/actions/workflows/ci.yml/badge.svg)](https://github.com/tox-dev/peryx/actions/workflows/ci.yml)
[![Documentation](https://img.shields.io/readthedocs/peryx?logo=readthedocs&logoColor=white)](https://peryx.readthedocs.io/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://opensource.org/licenses/MIT)

Peryx is an artifact cache and private registry. One executable serves each supported artifact format. Each `[[index]]`
selects a format through its `ecosystem` setting; `[availability].mode` selects `none`, `dc`, or `ha`. Unselected
formats add no routes or background work.

## Properties

- Each index role shares a content-addressed blob store.
- Cached indexes fetch on a miss, verify the digest, and serve later requests from local or object storage.
- Hosted indexes accept private uploads under scoped access controls.
- `availability.mode = "none"` starts no replication or coordination work. `dc` and `ha` add replication across the
  configured failure domains.

## Install and run

```shell
curl -LsSf https://github.com/tox-dev/peryx/releases/latest/download/peryx-installer.sh | sh
peryx serve
```

The process listens on `127.0.0.1:4433` by default. The
[configuration reference](https://peryx.readthedocs.io/en/latest/core/operations/configuration/) documents index owners
and availability modes.

Contributors can [build from a checkout](https://peryx.readthedocs.io/en/latest/contributing/build/).

## Documentation

- [Ecosystem docs](https://peryx.readthedocs.io/en/latest/ecosystems/) contain client commands, protocol settings, and
  behavior.
- [Contributor architecture](https://peryx.readthedocs.io/en/latest/contributing/architecture/) covers code ownership
  and startup lifecycle.
- [Contributing](CONTRIBUTING.md) lists local and CI checks.

## License

The [MIT license](https://opensource.org/licenses/MIT) covers peryx.
