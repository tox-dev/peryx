#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
cleanup() {
  chmod -R u+w "$scratch"
  rm -rf "$scratch"
}
trap cleanup EXIT

fixture="$scratch/repo"
mkdir -p "$fixture/scripts/ci" "$fixture/crates/owner/tests/conformance" "$scratch/bin"
cp "$repo/scripts/ci/conformance" "$fixture/scripts/ci/"
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '{"packages":[{"name":"owner","manifest_path":"%s/crates/owner/Cargo.toml","metadata":{"peryx-ci":{"conformance":{"repository":"https://example.invalid/spec","revision":"0123456789012345678901234567890123456789","directory":"conformance","command":["go","test","-c","-o","{output}"],"runner":"tests/conformance/run.sh"}}}}]}' "$FIXTURE"
EOF
cat >"$scratch/bin/git" <<'EOF'
#!/usr/bin/env bash
if [[ $1 == clone ]]; then
  mkdir -p "${@: -1}/conformance"
fi
EOF
cat >"$scratch/bin/go" <<'EOF'
#!/usr/bin/env bash
if [[ $1 == clean && $2 == -modcache ]]; then
  printf 'cleaned\n' >>"$CLEAN_MARKER"
  chmod -R u+w "$GOMODCACHE"
  rm -rf "$GOMODCACHE"
  exit "${GO_CLEAN_STATUS:-0}"
fi
while (($#)); do
  if [[ $1 == -o ]]; then
    touch "$2"
    break
  fi
  shift
done
mkdir -p "$GOMODCACHE/example/module"
touch "$GOMODCACHE/example/module/source.go"
chmod -R a-w "$GOMODCACHE"
EOF
cat >"$fixture/crates/owner/tests/conformance/run.sh" <<'EOF'
#!/usr/bin/env bash
[[ -x $1 || -f $1 ]]
[[ -f $2 ]]
exit "${RUNNER_STATUS:-0}"
EOF
chmod +x "$scratch/bin/"* "$fixture/crates/owner/tests/conformance/run.sh"
touch "$scratch/peryx"

run_harness() {
  CLEAN_MARKER="$scratch/cleaned" FIXTURE="$fixture" PATH="$scratch/bin:$PATH" RUNNER_STATUS=${1:-0} \
    bash "$fixture/scripts/ci/conformance" owner "$scratch/conformance.test" "$scratch/peryx"
}

assert_no_sessions() {
  if compgen -G "$fixture/.tox/tmp/conformance/session.*" >/dev/null; then
    printf 'conformance session was not removed\n' >&2
    exit 1
  fi
}

run_harness
[[ $(<"$scratch/cleaned") == cleaned ]]
assert_no_sessions

set +e
run_harness 23
status=$?
set -e
[[ $status == 23 ]]
(( $(wc -l <"$scratch/cleaned") == 2 ))
assert_no_sessions

set +e
GO_CLEAN_STATUS=17 run_harness
status=$?
set -e
[[ $status == 17 ]]
(( $(wc -l <"$scratch/cleaned") == 3 ))
assert_no_sessions
