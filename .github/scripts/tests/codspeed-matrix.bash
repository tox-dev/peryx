#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"
jq -n '{packages: [
  {name: "oci", metadata: {"peryx-ci": {codspeed: {
    label: "OCI", "change-key": "oci", order: 1, "cargo-jobs": 3,
    "runner-fallback": true, benches: []
  }}}},
  {name: "pypi", metadata: {"peryx-ci": {codspeed: {
    label: "PyPI", "change-key": "pypi", order: 0, "cargo-jobs": 2, benches: ["pypi"]
  }}}}
]}' >"$scratch/metadata.json"
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
cat "$CARGO_METADATA_FIXTURE"
EOF
chmod +x "$scratch/bin/cargo"
export CARGO_METADATA_FIXTURE="$scratch/metadata.json"
export PATH="$scratch/bin:$PATH"
cd "$repo"

matrix() {
  just codspeed-matrix "$@"
}

assert_fails() {
  local expected=$1 output
  shift
  if output=$(matrix "$@" 2>&1); then
    printf 'command unexpectedly passed: %s\n' "$*" >&2
    exit 1
  fi
  grep -Fxq "$expected" <<<"$output"
}

assert_matrix() {
  local name=$1 expected=$2 output
  shift 2
  output=$(matrix "$@")
  if [[ $output != "$expected" ]]; then
    printf '%s returned:\n%s\nexpected:\n%s\n' "$name" "$output" "$expected" >&2
    exit 1
  fi
}

all='[{"ecosystem":"PyPI","package":"pypi","cargo_jobs":2},'\
'{"ecosystem":"OCI","package":"oci","cargo_jobs":3}]'
pypi='[{"ecosystem":"PyPI","package":"pypi","cargo_jobs":2}]'
oci='[{"ecosystem":"OCI","package":"oci","cargo_jobs":3}]'
assert_matrix push "$all" push false false false false
assert_matrix workflow-dispatch "$all" workflow_dispatch false false false false
assert_matrix shared-core "$all" pull_request false true false false
assert_matrix pypi-only "$pypi" pull_request false false \
  --change pypi=true --change oci=false
assert_matrix oci-only "$oci" pull_request false false \
  --change pypi=false --change oci=true
assert_matrix runner-only "$oci" pull_request true false false false
assert_matrix test-only '[]' pull_request false false false false
assert_matrix docs-only '[]' pull_request false false \
  --change pypi=false --change oci=false
assert_fails 'change state must be true or false: changed' pull_request false false changed false
assert_fails 'change states must name every CodSpeed owner' pull_request false false --change pypi=true
assert_fails 'change keys must be unique' pull_request false false \
  --change pypi=true --change pypi=false

jq '.packages[0].metadata["peryx-ci"].codspeed["change-key"] = "pypi"' \
  "$scratch/metadata.json" >"$scratch/invalid-metadata.json"
export CARGO_METADATA_FIXTURE="$scratch/invalid-metadata.json"
assert_fails 'CodSpeed metadata has invalid or duplicate matrix fields' push false false false false
