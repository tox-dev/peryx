#!/usr/bin/env bash
set -euo pipefail

package=${1:?Rust package to benchmark}
jobs=${2:-4}

case "$package" in
  peryx-ecosystem-oci | peryx-ecosystem-pypi) ;;
  *) echo "unsupported benchmark package: $package" >&2; exit 2 ;;
esac

git config --global --add safe.directory "$(pwd)"
rebuilt=false
if [[ ${CODSPEED_FORCE_REBUILD:-false} == true ]]; then
  marker="target/codspeed/local-source-$package"
  if [[ ! -f "$marker" || $(< "$marker") != "${CODSPEED_SOURCE_KEY:-}" ]]; then
    cargo clean --profile release -p "$package"
    rebuilt=true
  fi
fi
cargo codspeed build --locked -j "$jobs" -m simulation -p "$package"
if [[ "$rebuilt" == true ]]; then
  printf '%s\n' "$CODSPEED_SOURCE_KEY" > "$marker"
fi
sha256sum "target/codspeed/analysis/$package"/*
codspeed_args=(run --mode simulation)
if [[ ${CODSPEED_SKIP_UPLOAD:-false} == true ]]; then
  codspeed_args+=(--skip-upload)
fi
codspeed "${codspeed_args[@]}" -- cargo codspeed run -p "$package"
