#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
PERYX_MMDC="$repo/site/node_modules/.bin/mmdc" \
  node "$repo/site/scripts/render_diagrams.mjs" --force
