#!/usr/bin/env bash
set -euo pipefail

for executable in cargo cargo-codspeed codspeed git jq perl sha256sum; do
  if ! command -v "$executable" >/dev/null; then
    printf 'CodSpeed runtime is missing %s\n' "$executable" >&2
    exit 127
  fi
done

run_with_timeout() {
  local seconds=$1
  shift
  local status=0
  perl -e 'alarm shift; exec @ARGV or die "exec: $!\n"' "$seconds" "$@" || status=$?
  if ((status == 142)); then
    printf 'command timed out after %s seconds: %s\n' "$seconds" "$*" >&2
    return 124
  fi
  return "$status"
}

package=${1:?Rust package to benchmark}
jobs=${2:-4}
build_timeout=${CODSPEED_BUILD_TIMEOUT_SECONDS:-1200}
run_timeout=${CODSPEED_RUN_TIMEOUT_SECONDS:-600}
if [[ ! $build_timeout =~ ^[1-9][0-9]*$ || ! $run_timeout =~ ^[1-9][0-9]*$ ]]; then
  printf 'CodSpeed process deadlines must be positive integers\n' >&2
  exit 2
fi

metadata=$(cargo metadata --no-deps --format-version 1)
if ! jq -e --arg package "$package" '
  any(.packages[]; .name == $package and .metadata["peryx-ci"].codspeed != null)
' <<<"$metadata" >/dev/null; then
  printf 'package has no CodSpeed metadata: %s\n' "$package" >&2
  exit 2
fi
bench_args=()
while IFS= read -r bench; do
  bench_args+=(--bench "$bench")
done < <(jq -r --arg package "$package" '
  .packages[] | select(.name == $package) | .metadata["peryx-ci"].codspeed.benches[]?
' <<<"$metadata")

git config --global --add safe.directory "$(pwd)"
rebuilt=false
if [[ ${CODSPEED_FORCE_REBUILD:-false} == true ]]; then
  marker="target/codspeed/local-source-$package"
  if [[ ! -f "$marker" || $(< "$marker") != "${CODSPEED_SOURCE_KEY:-}" ]]; then
    cargo clean --profile release -p "$package"
    rebuilt=true
  fi
fi
run_with_timeout "$build_timeout" cargo codspeed build --locked -j "$jobs" -m simulation \
  -p "$package" "${bench_args[@]}"
if [[ "$rebuilt" == true ]]; then
  printf '%s\n' "$CODSPEED_SOURCE_KEY" > "$marker"
fi
sha256sum "target/codspeed/analysis/$package"/*
if [[ ${CODSPEED_BUILD_ONLY:-false} == true ]]; then
  exit 0
fi
codspeed_args=(run --mode simulation)
if [[ ${CODSPEED_SKIP_UPLOAD:-false} == true ]]; then
  codspeed_args+=(--skip-upload)
fi
run_with_timeout "$run_timeout" codspeed "${codspeed_args[@]}" -- \
  cargo codspeed run -p "$package" "${bench_args[@]}"
