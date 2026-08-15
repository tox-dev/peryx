#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
mkdir -p "$repo/.tox/tmp"
scratch=$(mktemp -d "$repo/.tox/tmp/crate-contracts.XXXXXX")
scratch=$(cd "$scratch" && pwd -P)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/packages"
for package in tested benched checked portable rlib-tested rlib-disabled failed invalid; do
  mkdir -p "$scratch/packages/$package/src"
  printf 'pub fn %s() {}\n' "$package" >"$scratch/packages/$package/src/lib.rs"
  printf '[package]\nname = "%s"\nversion = "0.0.0"\n' "$package" >"$scratch/packages/$package/Cargo.toml"
done
for package in peryx-pypi-system-tests peryx-oci-system-tests; do
  mkdir -p "$scratch/packages/$package/src"
  printf 'pub fn system_test() {}\n' >"$scratch/packages/$package/src/lib.rs"
  printf '[package]\nname = "%s"\nversion = "0.0.0"\n' "$package" >"$scratch/packages/$package/Cargo.toml"
done
mkdir -p "$scratch/packages/tested/tests/fixtures"
printf '{"covered": true}\n' >"$scratch/packages/tested/tests/fixtures/response.json"
jq -n --arg root "$scratch/packages" '{workspace_root: $root, packages: [
  {name: "tested", manifest_path: ($root + "/tested/Cargo.toml"), metadata: {}, targets: [{test: true, doctest: true, kind: ["lib"]}]},
  {name: "benched", manifest_path: ($root + "/benched/Cargo.toml"), metadata: {}, targets: [
    {name: "first", test: false, doctest: false, kind: ["bench"]},
    {name: "second", test: false, doctest: false, kind: ["bench"]}
  ]},
  {name: "checked", manifest_path: ($root + "/checked/Cargo.toml"), metadata: {}, targets: [
    {name: "checked", test: false, doctest: false, kind: ["bin"]},
    {name: "reverse_dependent", test: false, doctest: false, kind: ["cdylib"], crate_types: ["cdylib"]}
  ]},
  {name: "portable", manifest_path: ($root + "/portable/Cargo.toml"), metadata: {
    "peryx-ci": {"crate-contract": {"test-features": []}}
  }, targets: [{test: true, doctest: false, kind: ["lib"]}]},
  {name: "rlib-tested", manifest_path: ($root + "/rlib-tested/Cargo.toml"), metadata: {}, targets: [{test: true, doctest: false, kind: ["cdylib", "rlib"]}]},
  {name: "rlib-disabled", manifest_path: ($root + "/rlib-disabled/Cargo.toml"), metadata: {}, targets: [{test: false, doctest: false, kind: ["rlib"]}]},
  {name: "failed", manifest_path: ($root + "/failed/Cargo.toml"), metadata: {}, targets: [{test: true, doctest: false, kind: ["lib"]}]},
  {name: "invalid", manifest_path: ($root + "/invalid/Cargo.toml"), metadata: {}, targets: [{test: true, doctest: false, kind: ["lib"]}]},
  {name: "peryx-pypi-system-tests", manifest_path: ($root + "/peryx-pypi-system-tests/Cargo.toml"), metadata: {"peryx-ci": {kind: "system"}}, targets: []},
  {name: "peryx-oci-system-tests", manifest_path: ($root + "/peryx-oci-system-tests/Cargo.toml"), metadata: {"peryx-ci": {kind: "system"}}, targets: []}
]}' >"$scratch/metadata.json"
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s|%s|%s\n' "${CARGO_LLVM_COV_TARGET_DIR:-}" "$*" "${CARGO_TARGET_DIR:-}" >>"$CARGO_LOG"
printf '%s|%s\n' "${LLVM_PROFILE_FILE:-}" "${CARGO_LLVM_COV_TARGET_DIR:-}" >>"$COVERAGE_ENV_LOG"
if [[ $* == *--no-clean* && $* == *--no-report* ]]; then
  printf 'error: --no-report may not be used together with --no-clean\n' >&2
  exit 2
fi
if [[ $1 == metadata ]]; then
  cat "$CARGO_METADATA_FIXTURE"
  exit
fi
package=
output=
target_dir=
previous=
for argument in "$@"; do
  if [[ $previous == -p ]]; then package=$argument; fi
  if [[ $previous == --output-path ]]; then output=$argument; fi
  if [[ $previous == --target-dir ]]; then target_dir=$argument; fi
  previous=$argument
done
if [[ $* == *'llvm-cov nextest'* ]]; then
  [[ $package != failed ]]
  exit
fi
if [[ $1 == clean ]]; then
  package_artifact=${package//-/_}
  find "$target_dir/debug/deps" -maxdepth 1 -type f \
    \( -name "lib$package_artifact-*" -o -name "$package-*" \) -delete
  exit
fi
if [[ -n ${BLOCK_BENCH:-} && $* == *'llvm-cov -p benched'* ]]; then
  read -r _blocked <"$BLOCK_BENCH_FIFO"
fi
if [[ $* == *'llvm-cov report'* ]]; then
  if [[ $package == invalid ]]; then
    : >"$output"
  else
    source="$PACKAGE_FIXTURES/$package/src/lib.rs"
    printf 'SF:%s\nFN:1,1,%s\nFNDA:1,%s\nDA:1,1\nend_of_record\n' \
      "$source" "$package" "$package" >"$output"
  fi
fi
EOF
chmod +x "$scratch/bin/cargo"
export CARGO_LOG="$scratch/cargo.log"
export COVERAGE_ENV_LOG="$scratch/coverage-env.log"
export CARGO_METADATA_FIXTURE="$scratch/metadata.json"
export PACKAGE_FIXTURES="$scratch/packages"
export CARGO_TARGET_DIR="$scratch/shared-target"
export PERYX_COVERAGE_TARGET_DIR="$CARGO_TARGET_DIR"
export CARGO_LLVM_COV_TARGET_DIR="$scratch/shared-target/llvm-cov-target"
export PATH="$scratch/bin:$PATH"

mkdir -p "$scratch/shared-target/llvm-cov-target/debug/deps"
coverage_target="$scratch/shared-target/llvm-cov-target"
stale_artifact="$coverage_target/debug/deps/libtested-stale.rlib"
dependency_artifact="$coverage_target/debug/deps/libdependency-stale.rlib"
normal_stale_artifact="$scratch/shared-target/debug/deps/libtested-stale.rlib"
normal_dependency_artifact="$scratch/shared-target/debug/deps/libdependency-stale.rlib"
embedded_artifact="$coverage_target/debug/deps/libreverse_dependent-stale.dylib"
normal_embedded_artifact="$scratch/shared-target/debug/deps/libreverse_dependent-stale.dylib"
unrelated_dynamic_library="$coverage_target/debug/deps/libunrelated-stale.dylib"
mkdir -p "$scratch/shared-target/debug/deps"
touch "$stale_artifact" "$dependency_artifact" \
  "$normal_stale_artifact" "$normal_dependency_artifact" \
  "$coverage_target/interrupted.profraw" "$coverage_target/interrupted.profdata" \
  "$coverage_target/interrupted-profraw-list"
printf '%s\n' "$scratch/packages/tested/src/lib.rs" >"$embedded_artifact"
printf '%s\n' "$scratch/packages/tested/src/lib.rs" >"$normal_embedded_artifact"
printf 'unrelated\n' >"$unrelated_dynamic_library"
output=$(cd "$repo" && \
  .github/scripts/crate-contracts "$scratch/success" tested benched checked portable rlib-tested rlib-disabled)
[[ ! -e $stale_artifact ]]
[[ -e $dependency_artifact ]]
[[ ! -e $normal_stale_artifact ]]
[[ -e $normal_dependency_artifact ]]
[[ ! -e $embedded_artifact ]]
[[ ! -e $normal_embedded_artifact ]]
[[ -e $unrelated_dynamic_library ]]
[[ ! -e $coverage_target/interrupted.profraw ]]
[[ ! -e $coverage_target/interrupted.profdata ]]
[[ ! -e $coverage_target/interrupted-profraw-list ]]
[[ $(grep -c '|metadata --no-deps --format-version 1|' "$CARGO_LOG") == 1 ]]
awk -F '|' -v profile="$coverage_target/peryx-%p-%10m.profraw" -v target="$coverage_target" \
  'NF != 2 || $1 != profile || $2 != target { exit 1 }' \
  "$COVERAGE_ENV_LOG"
[[ ! -e $coverage_target.lock ]]
for package in tested benched checked portable rlib-tested rlib-disabled; do
  grep -q "|check -p $package --all-targets --all-features|" "$CARGO_LOG"
done
grep -Fq "$coverage_target|llvm-cov nextest -p tested --all-features --lib --no-report|$scratch/shared-target" \
  "$CARGO_LOG"
grep -Fq "$coverage_target|llvm-cov nextest -p rlib-tested --all-features --lib --no-report|$scratch/shared-target" \
  "$CARGO_LOG"
grep -Fq "$coverage_target|llvm-cov nextest -p portable --lib --no-report|$scratch/shared-target" \
  "$CARGO_LOG"
if grep -q '|llvm-cov nextest -p rlib-disabled ' "$CARGO_LOG"; then
  printf 'test-disabled rlib target ran as a library test\n' >&2
  exit 1
fi
grep -Fq "$coverage_target|llvm-cov -p benched --all-features --bench first --no-report|$scratch/shared-target" \
  "$CARGO_LOG"
grep -Fq "$coverage_target|llvm-cov -p benched --all-features --bench second --no-report|$scratch/shared-target" \
  "$CARGO_LOG"
[[ $(grep -c '|clean -p .* --target-dir ' "$CARGO_LOG") == 12 ]]
[[ $(grep -c '|clean -p .* --target-dir .*llvm-cov-target|' "$CARGO_LOG") == 6 ]]
[[ $(grep -c '|llvm-cov clean --profraw-only|' "$CARGO_LOG") == 6 ]]
grep -q '|test -p tested --doc --all-features|' "$CARGO_LOG"
grep -q '|llvm-cov report -p tested --no-default-ignore-filename-regex --lcov' "$CARGO_LOG"
if grep -Eq -- '--no-clean.*--no-report|--no-report.*--no-clean' "$CARGO_LOG"; then
  printf 'incompatible coverage accumulation flags were used\n' >&2
  exit 1
fi
doctest_line=$(grep -n '|test -p tested --doc --all-features|' "$CARGO_LOG" | cut -d: -f1)
test_line=$(grep -n '|llvm-cov nextest -p tested --all-features --lib --no-report|' "$CARGO_LOG" | cut -d: -f1)
build_line=$(grep -n '|check -p tested --all-targets --all-features|' "$CARGO_LOG" | cut -d: -f1)
normal_clean_line=$(grep -nF "|clean -p tested --target-dir $scratch/shared-target|" "$CARGO_LOG" | cut -d: -f1)
coverage_clean_line=$(grep -nF "|clean -p tested --target-dir $coverage_target|" "$CARGO_LOG" | cut -d: -f1)
((normal_clean_line < coverage_clean_line && coverage_clean_line < build_line && \
  build_line < doctest_line && doctest_line < test_line))
[[ ! -e $scratch/success/profiles ]]
jq -se 'all(.[]; .status == "passed") and any(.[]; .phase == "metadata" and .package == null) and any(.[]; .phase == "build" and .package == "checked")' \
  "$scratch/success/timings.jsonl" >/dev/null
jq -se 'any(.[]; .phase == "bench first" and .package == "benched") and any(.[]; .phase == "bench second" and .package == "benched")' \
  "$scratch/success/timings.jsonl" >/dev/null
jq -e '.version == 2 and .policy_version == 1 and (.policy_sha256 | length) == 64' \
  "$scratch/success/tested.contract.json" >/dev/null
grep -q '^passed .*tested source inventory$' <<<"$output"
.github/scripts/coverage-contract verify --metadata "$scratch/metadata.json" \
  "$scratch/success/tested.contract.json" "$scratch/success/tested.lcov"
.github/scripts/coverage-contract verify-timings --metadata "$scratch/metadata.json" \
  "$scratch/success/timings.jsonl"
.github/scripts/coverage-contract verify-timings --metadata "$scratch/metadata.json" \
  --contracts-root "$scratch/success" "$scratch/success/timings.jsonl"
printf 'pub fn changed() {}\n' >>"$scratch/packages/tested/src/lib.rs"
if .github/scripts/coverage-contract verify --metadata "$scratch/metadata.json" \
  "$scratch/success/tested.contract.json" "$scratch/success/tested.lcov"; then
  printf 'stale coverage contract passed\n' >&2
  exit 1
fi
if .github/scripts/coverage-contract verify-timings --metadata "$scratch/metadata.json" \
  "$scratch/success/timings.jsonl"; then
  printf 'stale timing artifact passed\n' >&2
  exit 1
fi
.github/scripts/coverage-contract verify-timings --metadata "$scratch/metadata.json" \
  --contracts-root "$scratch/success" "$scratch/success/timings.jsonl"
cp "$scratch/success/timings.jsonl" "$scratch/changed-timings.jsonl"
jq -c 'if .package == "tested" then .input_sha256 = "invalid" else . end' \
  "$scratch/changed-timings.jsonl" >"$scratch/changed-timings.tmp"
mv "$scratch/changed-timings.tmp" "$scratch/changed-timings.jsonl"
if .github/scripts/coverage-contract verify-timings --metadata "$scratch/metadata.json" \
  --contracts-root "$scratch/success" "$scratch/changed-timings.jsonl"; then
  printf 'timing digest mismatch passed\n' >&2
  exit 1
fi
sed -i.bak '$d' "$scratch/packages/tested/src/lib.rs"
rm -f "$scratch/packages/tested/src/lib.rs.bak"
printf '{"covered": false}\n' >"$scratch/packages/tested/tests/fixtures/response.json"
if .github/scripts/coverage-contract verify --metadata "$scratch/metadata.json" \
  "$scratch/success/tested.contract.json" "$scratch/success/tested.lcov"; then
  printf 'stale non-Rust fixture contract passed\n' >&2
  exit 1
fi
printf '{"covered": true}\n' >"$scratch/packages/tested/tests/fixtures/response.json"

policy_root="$scratch/policy"
policy_inputs=(
  .github/scripts/check-coverage-contracts
  .github/scripts/check-lcov-functions.awk
  .github/scripts/check-lcov-lines.awk
  .github/scripts/check-lcov-sources
  .github/scripts/coverage-availability
  .github/scripts/coverage-combine
  .github/scripts/coverage-contract
  .github/scripts/coverage-e2e
  .github/scripts/coverage-e2e-live
  .github/scripts/coverage-frontend
  .github/scripts/coverage-linux
  .github/scripts/coverage-merge
  .github/scripts/coverage-native
  .github/scripts/coverage-session-guard
  .github/scripts/coverage-simulation
  .github/scripts/coverage-system-clients
  .github/scripts/coverage-system-distributed
  .github/scripts/coverage-system-storage
  .github/scripts/coverage-toolchain
  .github/scripts/crate-contracts
  .github/scripts/package-contract-inputs
  .github/scripts/package-source-roots
  .github/workflows/ci.yml
  compose.yaml
  justfile
  nextest/config.toml
  scripts/ci/coverage-system-clients
  scripts/ci/coverage-system-distributed
  scripts/ci/coverage-system-suite
  scripts/ci/workspace-package-list
  scripts/ci/workspace-packages
)
for input in "${policy_inputs[@]}"; do
  mkdir -p "$policy_root/$(dirname -- "$input")"
  cp "$repo/$input" "$policy_root/$input"
done
.github/scripts/coverage-contract write --metadata "$scratch/metadata.json" \
  --policy-root "$policy_root" "$scratch/success/tested.lcov" \
  "$scratch/policy.contract.json" tested
.github/scripts/coverage-contract verify --metadata "$scratch/metadata.json" \
  --policy-root "$policy_root" "$scratch/policy.contract.json" "$scratch/success/tested.lcov"
printf '\n' >>"$policy_root/justfile"
if .github/scripts/coverage-contract verify --metadata "$scratch/metadata.json" \
  --policy-root "$policy_root" "$scratch/policy.contract.json" "$scratch/success/tested.lcov"; then
  printf 'stale coverage policy passed\n' >&2
  exit 1
fi

cp "$scratch/success/tested.contract.json" "$scratch/stale.contract.json"
if .github/scripts/coverage-contract write --metadata "$scratch/metadata.json" \
  --expected-digest invalid "$scratch/success/tested.lcov" "$scratch/stale.contract.json" tested; then
  printf 'changed-input contract write passed\n' >&2
  exit 1
fi
[[ ! -e $scratch/stale.contract.json ]]

printf 'stale\n' >"$scratch/stale.lcov"
: >"$scratch/empty.lcov"
if .github/scripts/coverage-combine "$scratch/stale.lcov" "$scratch/empty.lcov"; then
  printf 'empty coverage input passed\n' >&2
  exit 1
fi
[[ ! -e $scratch/stale.lcov ]]

: >"$CARGO_LOG"
mkdir -p "$scratch/failure"
printf 'stale\n' >"$scratch/failure/failed.lcov"
if output=$(cd "$repo" && .github/scripts/crate-contracts "$scratch/failure" failed 2>&1); then
  printf 'failed contract passed\n' >&2
  exit 1
fi
[[ ! -e $scratch/failure ]]
if grep -q '|llvm-cov report -p failed ' "$CARGO_LOG"; then
  printf 'failed instrumentation generated a coverage report\n' >&2
  exit 1
fi
if grep -Eq 'source inventory|function coverage|line coverage|source functions covered|Rust sources present' \
  <<<"$output"; then
  printf 'failed instrumentation ran exact coverage gates\n' >&2
  exit 1
fi
for unsafe in COVERAGE_NO_CLEAN COVERAGE_NO_REPORT; do
  if (cd "$repo" && env "$unsafe=1" .github/scripts/crate-contracts "$scratch/unsafe" tested); then
    printf 'unsafe accumulated profile flag passed: %s\n' "$unsafe" >&2
    exit 1
  fi
done
.github/scripts/coverage-session-guard
if COVERAGE_NO_CLEAN=1 .github/scripts/coverage-session-guard; then
  printf 'unguarded profile accumulation passed\n' >&2
  exit 1
fi
PERYX_COVERAGE_SESSION=linux COVERAGE_NO_CLEAN=1 .github/scripts/coverage-session-guard \
  --output "$scratch/guard" -- true

guard_repo="$scratch/guard-repo"
mkdir -p "$guard_repo/.github/scripts"
cp .github/scripts/coverage-session-guard "$guard_repo/.github/scripts/"
target_log="$scratch/guard-target"
target_recorder="$scratch/record-coverage-target"
cat >"$target_recorder" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$CARGO_TARGET_DIR" >"$1"
EOF
chmod +x "$target_recorder"
env -u PERYX_COVERAGE_TARGET_DIR -u CARGO_LLVM_COV_TARGET_DIR \
  CARGO_TARGET_DIR="$scratch/ambient-target" \
  "$guard_repo/.github/scripts/coverage-session-guard" --output "$scratch/isolated" -- \
  "$target_recorder" "$target_log"
[[ $(cat "$target_log") == "$guard_repo/.tox/coverage-target" ]]
[[ ! -e $scratch/ambient-target ]]

mkdir -p "$scratch/locked-target/llvm-cov-target.lock"
printf '%s\n' "$$" >"$scratch/locked-target/llvm-cov-target.lock/pid"
if PERYX_COVERAGE_TARGET_DIR="$scratch/locked-target" \
  CARGO_LLVM_COV_TARGET_DIR="$scratch/locked-target/llvm-cov-target" \
  .github/scripts/coverage-session-guard \
  --output "$scratch/locked" -- true; then
  printf 'concurrent coverage target passed\n' >&2
  exit 1
fi
rm -rf "$scratch/locked-target/llvm-cov-target.lock"

printf 'peryx-pypi-system-tests\nperyx-oci-system-tests\n' >"$scratch/system-packages"
printf 'SF:%s\nFN:1,1,system_test\nFNDA:1,system_test\nDA:1,1\nend_of_record\n' \
  "$scratch/packages/peryx-pypi-system-tests/src/lib.rs" >"$scratch/success/system.lcov"
.github/scripts/coverage-contract write --metadata "$scratch/metadata.json" \
  "$scratch/success/system.lcov" "$scratch/success/peryx-pypi-system-tests.contract.json" \
  peryx-pypi-system-tests
if .github/scripts/check-coverage-contracts --metadata "$scratch/metadata.json" \
  --packages-file "$scratch/system-packages" "$scratch/success"; then
  printf 'missing system package contract passed\n' >&2
  exit 1
fi

if (cd "$repo" && .github/scripts/crate-contracts "$scratch/invalid" invalid); then
  printf 'invalid coverage report passed\n' >&2
  exit 1
fi
if (cd "$repo" && .github/scripts/crate-contracts "$scratch/unknown" absent); then
  printf 'unknown package passed\n' >&2
  exit 1
fi
if (cd "$repo" && .github/scripts/crate-contracts "$scratch/empty"); then
  printf 'empty package list passed\n' >&2
  exit 1
fi
mkfifo "$scratch/blocked-benchmark"
if output=$(cd "$repo" && BLOCK_BENCH=1 BLOCK_BENCH_FIFO="$scratch/blocked-benchmark" \
  BENCH_TIMEOUT_SECONDS=1 \
  .github/scripts/crate-contracts "$scratch/timeout" benched 2>&1); then
  printf 'timed-out benchmark passed\n' >&2
  exit 1
fi
grep -q 'command timed out after 1 seconds' <<<"$output"
