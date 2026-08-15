#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
scratch=$(cd "$scratch" && pwd -P)
trap 'rm -rf "$scratch"' EXIT
fixture="$scratch/repo with spaces"
mkdir -p "$scratch/bin" "$fixture/.github/scripts" "$fixture/scripts/ci" "$fixture/crates/peryx"
cp "$repo/.github/scripts/coverage-session-guard" "$fixture/.github/scripts/"
cp "$repo/scripts/ci/coverage-system-distributed" "$fixture/scripts/ci/"
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo|%s|%s|%s\n' "${LLVM_PROFILE_FILE:-}" "$CARGO_LLVM_COV_TARGET_DIR" "$*" >>"$CALL_LOG"
if [[ $1 == metadata ]]; then
  cat "$CARGO_METADATA_FIXTURE"
  exit
fi
if [[ -n ${SOURCE_PROFILE:-} && $* == *'llvm-cov nextest'* ]]; then
  touch "$SOURCE_PROFILE"
fi
if [[ -n ${FAIL_COVERAGE:-} && $* == *'llvm-cov nextest'* ]]; then
  exit 7
fi
if [[ $* == *'llvm-cov report'* ]]; then
  output=
  previous=
  for argument in "$@"; do
    if [[ $previous == --output-path ]]; then output=$argument; fi
    previous=$argument
  done
  [[ -d $(dirname -- "$output") ]]
  touch "$output"
fi
EOF
cat >"$fixture/scripts/ci/coverage-system-suite" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'suite|%s|%s|%s|%s\n' "${COVERAGE_NO_CLEAN:-}" "${PERYX_COVERAGE_SESSION:-}" \
  "$PERYX_COVERAGE_RUN_DIR" "$*" >>"$CALL_LOG"
EOF
chmod +x "$scratch/bin/cargo" "$fixture/.github/scripts/coverage-session-guard" \
  "$fixture/scripts/ci/coverage-system-distributed" "$fixture/scripts/ci/coverage-system-suite"
export CALL_LOG="$scratch/calls.log"
export CARGO_METADATA_FIXTURE="$scratch/metadata.json"
export PATH="$scratch/bin:$PATH"
cat >"$CARGO_METADATA_FIXTURE" <<EOF
{"packages":[{"name":"storage","metadata":{"peryx-ci":{"coverage":{"clients":{}} ,"tools":[]}}}]}
EOF

: >"$CALL_LOG"
output="$scratch/output with spaces/fresh.lcov"
(cd "$fixture" && env -u COVERAGE_NO_CLEAN -u PERYX_COVERAGE_SESSION \
  scripts/ci/coverage-system-distributed "$output")
[[ $(grep -c 'llvm-cov clean --workspace$' "$CALL_LOG") == 1 ]]
grep -q '|llvm-cov nextest ' "$CALL_LOG"
grep -q '^suite|1||.*|availability '"$output"'$' "$CALL_LOG"
grep -q '|llvm-cov report .* --output-path '"$output"'$' "$CALL_LOG"
awk -F '|' 'BEGIN { status = 0 } $1 == "cargo" {
  if ($2 !~ /\/llvm-cov-target\/peryx-%p-%10m[.]profraw$/ ||
      $3 !~ /\/[.]tox\/coverage-sessions\/[^\/]+\/llvm-cov-target$/) status = 1
} END { exit status }' "$CALL_LOG"
[[ -z $(find "$fixture/.tox/coverage-sessions" -mindepth 1 -print -quit) ]]

: >"$CALL_LOG"
(cd "$fixture" && PERYX_COVERAGE_SESSION=linux COVERAGE_NO_CLEAN=1 \
  scripts/ci/coverage-system-distributed "$scratch/accumulated.lcov")
if grep -q 'llvm-cov clean --workspace$' "$CALL_LOG"; then
  printf 'approved accumulation cleaned coverage profiles\n' >&2
  exit 1
fi
grep -q '|llvm-cov nextest ' "$CALL_LOG"
grep -q '^suite|1|linux|.*|availability '"$scratch/accumulated.lcov"'$' "$CALL_LOG"
grep -q '|llvm-cov report .* --output-path '"$scratch/accumulated.lcov"'$' "$CALL_LOG"

: >"$CALL_LOG"
if (cd "$fixture" && PERYX_COVERAGE_SESSION=local COVERAGE_NO_CLEAN=1 \
  scripts/ci/coverage-system-distributed "$scratch/unsafe.lcov"); then
  printf 'unsafe coverage accumulation passed\n' >&2
  exit 1
fi
[[ ! -s $CALL_LOG ]]

: >"$CALL_LOG"
if (cd "$fixture" && FAIL_COVERAGE=1 scripts/ci/coverage-system-distributed "$scratch/failed.lcov"); then
  printf 'failed coverage command passed\n' >&2
  exit 1
fi
[[ -z $(find "$fixture/.tox/coverage-sessions" -mindepth 1 -print -quit) ]]

: >"$CALL_LOG"
source_profile="$fixture/crates/peryx/default_generated.profraw"
if (cd "$fixture" && SOURCE_PROFILE="$source_profile" \
  scripts/ci/coverage-system-distributed "$scratch/leaked.lcov"); then
  printf 'source-tree coverage profile passed\n' >&2
  exit 1
fi
[[ -f $source_profile ]]
rm "$source_profile"

: >"$CALL_LOG"
touch "$source_profile"
if (cd "$fixture" && scripts/ci/coverage-system-distributed "$scratch/preexisting.lcov"); then
  printf 'pre-existing source-tree coverage profile passed\n' >&2
  exit 1
fi
[[ ! -s $CALL_LOG ]]

suite_fixture="$scratch/suite repo"
mkdir -p "$suite_fixture/scripts/ci" "$suite_fixture/.tox/tools/bin"
cp "$repo/scripts/ci/coverage-system-clients" "$repo/scripts/ci/coverage-system-suite" \
  "$suite_fixture/scripts/ci/"
cat >"$suite_fixture/scripts/ci/package-tools" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'tools|%s\n' "$*" >>"$CALL_LOG"
EOF
chmod +x "$suite_fixture/scripts/ci/coverage-system-clients" \
  "$suite_fixture/scripts/ci/coverage-system-suite" "$suite_fixture/scripts/ci/package-tools"

: >"$CALL_LOG"
suite_output="$scratch/suite output/nested/report.lcov"
(cd "$suite_fixture" && CARGO_LLVM_COV_TARGET_DIR="$scratch/llvm-cov-target" \
  PERYX_COVERAGE_RUN_DIR="$scratch/run" \
  scripts/ci/coverage-system-suite clients "$suite_output")
[[ -f $suite_output ]]

: >"$CALL_LOG"
clients_output="$scratch/clients output/nested/report.lcov"
(cd "$suite_fixture" && CARGO_LLVM_COV_TARGET_DIR="$scratch/llvm-cov-target" \
  PERYX_COVERAGE_RUN_DIR="$scratch/run" \
  scripts/ci/coverage-system-clients "$clients_output")
[[ -f $clients_output ]]
grep -q '^tools|storage$' "$CALL_LOG"
