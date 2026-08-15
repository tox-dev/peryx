#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
sed -n '/^clean_wasm_ir()/,/^}/p' "$repo/.github/scripts/coverage-frontend" >"$scratch/clean-wasm-ir"
# shellcheck source=/dev/null  # The generated path does not exist during static analysis.
source "$scratch/clean-wasm-ir"

wasm_ir="$scratch/front/wasm32-unknown-unknown/debug/deps"
mkdir -p "$wasm_ir/nested"
touch "$wasm_ir/peryx_web-stale.ll" "$wasm_ir/nested/dependency-stale.ll" "$wasm_ir/peryx_web.rlib"
clean_wasm_ir "$wasm_ir"
[[ -f $wasm_ir/peryx_web.rlib ]]
[[ -z $(find "$wasm_ir" -type f -name '*.ll' -print -quit) ]]

touch "$wasm_ir/peryx_web-current.ll"
[[ $(find "$wasm_ir" -type f -name '*.ll' -print) == "$wasm_ir/peryx_web-current.ll" ]]

cleanup_line=$(awk '/^clean_wasm_ir "\$wasm_ir"$/ { print NR; exit }' "$repo/.github/scripts/coverage-frontend")
build_line=$(awk '/cargo leptos build / { print NR; exit }' "$repo/.github/scripts/coverage-frontend")
[[ -n $cleanup_line && -n $build_line && $cleanup_line -lt $build_line ]]

object_line=$(awk '/"\$clang" --target=/ { print NR; exit }' "$repo/.github/scripts/coverage-frontend")
merge_line=$(awk '/"\$LLVM_PROFDATA" merge / { print NR; exit }' "$repo/.github/scripts/coverage-frontend")
[[ -n $object_line && -n $merge_line && $object_line -lt $merge_line ]]
