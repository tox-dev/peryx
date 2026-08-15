#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
npm --prefix "$repo/site" run render
mkdir -p "$repo/.tox/site/static"
cargo run --quiet --manifest-path "$repo/Cargo.toml" --package peryx --bin peryx -- openapi \
  >"$repo/.tox/site/static/openapi.json"
zola --root "$repo/.tox/site" serve --interface 127.0.0.1
