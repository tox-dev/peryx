#!/usr/bin/env bash
set -euo pipefail

package=${1:?Rust package to benchmark}
jobs=${2:-2}

case "$package" in
  peryx-ecosystem-oci | peryx-ecosystem-pypi) ;;
  *) echo "unsupported benchmark package: $package" >&2; exit 2 ;;
esac

cargo codspeed build --locked -j "$jobs" -m simulation -p "$package"
sha256sum "target/codspeed/analysis/$package"/*
codspeed run --mode simulation -- cargo codspeed run -p "$package"
