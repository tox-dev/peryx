#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
script="$repo/.github/scripts/coverage-linux"
grep -Fq '.github/scripts/coverage-system-clients' "$script"
grep -Fq '.github/scripts/coverage-system-distributed' "$script"
grep -Fq '.github/scripts/coverage-system-storage' "$script"
grep -Fq 'export COVERAGE_NO_REPORT=1' "$script"
[[ $(grep -c 'cargo llvm-cov report' "$script") == 1 ]]
for superseded in coverage-e2e coverage-e2e-live coverage-availability coverage-simulation; do
  if grep -Fq ".github/scripts/$superseded" "$script"; then
    printf 'coverage-linux calls superseded lane: %s\n' "$superseded" >&2
    exit 1
  fi
done
