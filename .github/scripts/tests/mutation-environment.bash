#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
source "$repo/.github/scripts/mutation-environment"
scratch=$(TMPDIR=/tmp mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT

RUNNER_TEMP="$scratch/runner"
TMPDIR="$repo/.tox/tmp"
mutation_environment "$repo"
[[ $TMPDIR == "$(cd -- "$RUNNER_TEMP" && pwd -P)" ]]

if PERYX_MUTANTS_TMPDIR="$repo/.tox/tmp" mutation_environment "$repo" 2>/dev/null; then
  printf 'mutation scratch accepted a checkout path\n' >&2
  exit 1
fi

mkdir -p "$scratch/repo/.github/scripts" "$scratch/bin" "$scratch/capture"
cp "$repo/.github/scripts/mutation-environment" "$repo/.github/scripts/mutation-diff" \
  "$scratch/repo/.github/scripts/"
git -C "$scratch/repo" init -q
git -C "$scratch/repo" -c user.name=test -c user.email=test@example.com \
  commit --allow-empty -qm baseline
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$TMPDIR" >"$CAPTURE/tmpdir"
printf '%s\n' "$@" >"$CAPTURE/arguments"
EOF
chmod +x "$scratch/bin/cargo"
PATH="$scratch/bin:$PATH" CAPTURE="$scratch/capture" RUNNER_TEMP="$scratch/runner" \
  TMPDIR="$scratch/repo/.tox/tmp" "$scratch/repo/.github/scripts/mutation-diff" HEAD 1
[[ $(cat "$scratch/capture/tmpdir") == "$(cd -- "$scratch/runner" && pwd -P)" ]]
grep -Fxq -- '--output' "$scratch/capture/arguments"
grep -Fxq -- '.tox/mutants' "$scratch/capture/arguments"
grep -Fxq -- '--jobs' "$scratch/capture/arguments"
if grep -Fxq -- '--in-place' "$scratch/capture/arguments"; then
  printf 'local mutation used the checkout in place\n' >&2
  exit 1
fi

PATH="$scratch/bin:$PATH" CAPTURE="$scratch/capture" RUNNER_TEMP="$scratch/runner" \
  TMPDIR="$scratch/repo/.tox/tmp" "$scratch/repo/.github/scripts/mutation-diff" HEAD 1 true
grep -Fxq -- '--in-place' "$scratch/capture/arguments"
if grep -Fxq -- '--jobs' "$scratch/capture/arguments"; then
  printf 'in-place mutation set incompatible jobs\n' >&2
  exit 1
fi
