# velox

A PyPI-compatible read-through cache, written in Rust. Point pip, uv, or any PEP 503/691 client at velox and it proxies
and permanently caches a real index (pypi.org by default, or a private Artifactory, GitLab, or devpi mirror), serving
artifacts from a content-addressed store on the next request.

velox is small and fast: an async server (axum/tokio), a pure-Rust embedded metadata store (redb), and a
content-addressed blob cache. It speaks the wire protocols pip and uv already use, so no client changes are needed
beyond the index URL.

## Status

Early, but the read-through cache works end to end: `uv pip install` and `pip install` resolve and install through velox
against pypi.org, and every artifact is cached content-addressed on disk.

Implemented so far:

- `serve` and `init` commands, configured by flags, environment, or a TOML file.
- The [Simple Repository API][simple-api] at `/{index}/simple/` (project list) and `/{index}/simple/{project}/`
  (detail), negotiated between [PEP 691][pep691] JSON and [PEP 503][pep503] HTML, versioned per [PEP 629][pep629] and
  carrying the [PEP 700][pep700] `versions`/`size`/`upload-time` fields and [PEP 592][pep592] yank markers.
- File download with content-addressed caching and sha256 verification (the `#sha256=` fragment from [PEP 503][pep503]).
- [PEP 658][pep658]/[PEP 714][pep714] `.metadata` siblings: advertised, and served by fetching the upstream sibling,
  verifying it against the advertised digest, and caching it, so pip and uv get the metadata fast-path for resolution.
- Proxying HTML-only upstreams (Artifactory, GitLab, static indexes) by parsing their [PEP 503][pep503] HTML and
  re-serving it as [PEP 691][pep691] JSON.
- Per-upstream authentication (Basic or Bearer), including the pypi.org `__token__` convention from the
  [`.pypirc` spec][pypirc], and a configurable mirror index route and cache freshness window.
- Structured, leveled logging to stdout, a file, journald, or syslog.

Not yet built: private overlay indexes and upload (the [legacy upload API][upload-api]), the web UI, and distribution as
PyPI wheels or standalone installers. [proposal.md](proposal.md) holds the full design and the phased plan.

## Install

From source, with a Rust toolchain (the version is pinned in `rust-toolchain.toml`):

```shell
cargo build --release
# the binary is at target/release/velox
```

`pip install velox` and standalone installers are planned.

## Quick start

Initialize a data directory and start the server:

```shell
velox init --data-dir ./velox-data
velox serve --data-dir ./velox-data --port 4433
```

Then install through it. The built-in pypi.org mirror is exposed under the `root/pypi` index:

```shell
uv pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ six
# or
pip install --index-url http://127.0.0.1:4433/root/pypi/simple/ six
```

On a miss velox fetches from the upstream, verifies and caches the artifact, and serves it. Later requests are served
from the cache without touching the upstream.

## Configuration

Values resolve with the precedence `defaults < TOML file < environment < flags`.

| Setting                   | Flag              | Environment               | Default                    |
| ------------------------- | ----------------- | ------------------------- | -------------------------- |
| Bind host                 | `--host`          | `VELOX_HOST`              | `127.0.0.1`                |
| Bind port                 | `--port`          | `VELOX_PORT`              | `4433`                     |
| Data directory            | `--data-dir`      | `VELOX_DATA_DIR`          | `velox-data`               |
| Upstream index URL        | (env only)        | `VELOX_UPSTREAM_URL`      | `https://pypi.org/simple/` |
| Upstream username         | (env only)        | `VELOX_UPSTREAM_USERNAME` | (none)                     |
| Upstream password         | (env only)        | `VELOX_UPSTREAM_PASSWORD` | (none)                     |
| Upstream bearer token     | (env only)        | `VELOX_UPSTREAM_TOKEN`    | (none)                     |
| Mirror index route        | (env only)        | `VELOX_INDEX`             | `root/pypi`                |
| Cache freshness (seconds) | (env only)        | `VELOX_CACHE_TTL_SECS`    | `1800`                     |
| Config file               | `--config` / `-c` | (n/a)                     | (none)                     |

A TOML config file covers the same settings:

```toml
host = "0.0.0.0"
port = 4433
data_dir = "/var/lib/velox"
upstream_url = "https://pypi.org/simple/"

[log]
level = "info"
format = "pretty"
sink = "stdout"
```

## Proxying a private mirror

Point the upstream at any PEP 503 index and supply credentials. A bearer token takes precedence over a username/password
pair:

```shell
# Artifactory or GitLab access token (Bearer)
export VELOX_UPSTREAM_URL="https://myco.jfrog.io/artifactory/api/pypi/pypi/simple/"
export VELOX_UPSTREAM_TOKEN="<access-token>"

# or Basic auth (for example a pypi.org token)
export VELOX_UPSTREAM_USERNAME="__token__"
export VELOX_UPSTREAM_PASSWORD="<token>"
```

velox handles indexes that serve only HTML: it parses their PEP 503 page and re-serves it to clients as JSON, so uv and
pip get the modern format even from an old upstream.

## Logging

The log level comes from `--log-level {error,warn,info,debug,trace}` or `RUST_LOG`/`VELOX_LOG`, and can target a single
module (for example `velox_upstream=debug`). `-v` raises it to debug and `-vv` to trace. Output goes to one sink,
selected with `--log-sink`:

- `stdout` (default), pretty for a terminal or JSON with `--log-format json`
- `file`, a daily-rotating file at `--log-file <path>`
- `journald` on Linux, or `syslog`

## Endpoints

| Path                                     | Purpose                                 |
| ---------------------------------------- | --------------------------------------- |
| `GET /{index}/simple/`                   | Project list (JSON or HTML by `Accept`) |
| `GET /{index}/simple/{project}/`         | Project detail                          |
| `GET /{index}/files/{sha256}/{filename}` | Cached artifact                         |
| `GET /+status`                           | JSON health and identity                |
| `GET /metrics`                           | Prometheus metrics                      |

The built-in mirror index is `root/pypi`.

## Standards

velox targets the Python packaging interoperability standards a modern index and its clients rely on:

- [Simple Repository API][simple-api], the consolidated living spec (currently serving `meta.api-version` 1.1).
- [PEP 503][pep503], the HTML simple index and name normalization.
- [PEP 691][pep691], the JSON simple index and content negotiation.
- [PEP 629][pep629], simple API versioning.
- [PEP 700][pep700], the `versions`, `size`, and `upload-time` fields.
- [PEP 592][pep592], yanked releases.
- [PEP 658][pep658] and [PEP 714][pep714], the `.metadata` sibling, fetched from the upstream, verified, and served.
- [PEP 440][pep440], version identifiers and ordering.
- [PEP 508][pep508], dependency specifiers.
- [PEP 427][pep427] and [PEP 625][pep625], wheel and sdist filenames.
- [Core metadata][core-metadata], the `METADATA`/`PKG-INFO` fields.
- The [legacy upload API][upload-api] and [`.pypirc`][pypirc] authentication, for the upcoming upload path.

## Development

```shell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo llvm-cov --workspace --ignore-filename-regex 'main\.rs' --fail-under-lines 100 --fail-under-functions 100
```

velox holds 100% line and function coverage. The design, conventions, and roadmap live in [proposal.md](proposal.md).

## License

MIT.

[core-metadata]: https://packaging.python.org/en/latest/specifications/core-metadata/
[pep427]: https://packaging.python.org/en/latest/specifications/binary-distribution-format/
[pep440]: https://packaging.python.org/en/latest/specifications/version-specifiers/
[pep503]: https://peps.python.org/pep-0503/
[pep508]: https://packaging.python.org/en/latest/specifications/dependency-specifiers/
[pep592]: https://peps.python.org/pep-0592/
[pep625]: https://packaging.python.org/en/latest/specifications/source-distribution-format/
[pep629]: https://peps.python.org/pep-0629/
[pep658]: https://peps.python.org/pep-0658/
[pep691]: https://peps.python.org/pep-0691/
[pep700]: https://peps.python.org/pep-0700/
[pep714]: https://peps.python.org/pep-0714/
[pypirc]: https://packaging.python.org/en/latest/specifications/pypirc/
[simple-api]: https://packaging.python.org/en/latest/specifications/simple-repository-api/
[upload-api]: https://docs.pypi.org/api/upload/
