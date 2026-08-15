#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch_root=${PERYX_TEST_TMPDIR:-"$repo/.tox/tmp"}
mkdir -p "$scratch_root"
scratch=$(mktemp -d "$scratch_root/cleanup-workspace-artifacts.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT

new_fixture() {
  fixture="$scratch/$1"
  mkdir -p \
    "$fixture/scripts/ci" \
    "$fixture/.tox/coverage-target" \
    "$fixture/.tox/docker/data" \
    "$fixture/.tox/frontend/shared" \
    "$fixture/.tox/hawk/graph" \
    "$fixture/.tox/hawk/target" \
    "$fixture/.tox/reusable" \
    "$fixture/.tox/tmp"
  cp "$repo/scripts/ci/cleanup-workspace-artifacts" "$fixture/scripts/ci/"
  printf artifact >"$fixture/.tox/coverage-target/dependency.rlib"
  printf artifact >"$fixture/.tox/coverage-target/stale.profraw"
  printf artifact >"$fixture/.tox/docker/data/artifact"
  printf artifact >"$fixture/.tox/frontend/shared/artifact"
  printf artifact >"$fixture/.tox/hawk/graph/artifact"
  printf artifact >"$fixture/.tox/hawk/target/artifact"
  printf artifact >"$fixture/.tox/reusable/artifact"
  printf artifact >"$fixture/.tox/tmp/artifact"
  cat >"$fixture/scripts/ci/compose-run" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"$PERYX_COMPOSE_MARKER"
EOF
  chmod +x "$fixture/scripts/ci/compose-run"
}

assert_exists() {
  [[ -e $1 ]] || {
    printf 'expected path to exist: %s\n' "$1" >&2
    exit 1
  }
}

assert_absent() {
  [[ ! -e $1 ]] || {
    printf 'expected path to be absent: %s\n' "$1" >&2
    exit 1
  }
}

new_fixture coverage
bash "$fixture/scripts/ci/cleanup-workspace-artifacts" coverage
assert_exists "$fixture/.tox/coverage-target/dependency.rlib"
assert_absent "$fixture/.tox/coverage-target/stale.profraw"
assert_exists "$fixture/.tox/frontend/shared/artifact"
assert_exists "$fixture/.tox/hawk/graph/artifact"
assert_exists "$fixture/.tox/hawk/target/artifact"
assert_exists "$fixture/.tox/tmp/artifact"
assert_exists "$fixture/.tox/reusable/artifact"

new_fixture normal
PERYX_COMPOSE_MARKER="$fixture/compose-called" \
  bash "$fixture/scripts/ci/cleanup-workspace-artifacts" normal
assert_exists "$fixture/.tox/coverage-target/dependency.rlib"
assert_absent "$fixture/.tox/coverage-target/stale.profraw"
assert_absent "$fixture/.tox/frontend/shared"
assert_absent "$fixture/.tox/hawk/graph"
assert_absent "$fixture/.tox/tmp"
assert_exists "$fixture/.tox/hawk/target/artifact"
assert_exists "$fixture/.tox/docker/data/artifact"
assert_exists "$fixture/.tox/reusable/artifact"
assert_exists "$fixture/compose-called"

new_fixture all
PERYX_COMPOSE_MARKER="$fixture/compose-called" \
  bash "$fixture/scripts/ci/cleanup-workspace-artifacts" all
assert_absent "$fixture/.tox/coverage-target"
assert_absent "$fixture/.tox/docker"
assert_absent "$fixture/.tox/frontend"
assert_absent "$fixture/.tox/hawk/graph"
assert_absent "$fixture/.tox/hawk/target"
assert_exists "$fixture/.tox/reusable/artifact"
assert_exists "$fixture/compose-called"
[[ $(<"$fixture/compose-called") == clean ]] || {
  printf 'expected compose cleanup\n' >&2
  exit 1
}
