#!/usr/bin/env bash
# sha512 cases are optional because peryx stores sha256 blobs.
set -euo pipefail

peryx=${1:?path to the peryx binary}
conformance=${2:?path to the conformance.test binary}

port=18102
work=$(mktemp -d)
cleanup() {
  if [[ -n ${server_pid:-} ]]; then
    kill "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

cat >"$work/peryx.toml" <<EOF
host = "127.0.0.1"
port = $port
data_dir = "$work/data"

[[index]]
name = "store"
route = "store"
ecosystem = "oci"
hosted = true

[[index.access_token]]
name = "uploader"
secret = "conformance"
actions = ["write", "delete"]
EOF

"$peryx" serve --config "$work/peryx.toml" >"$work/server.log" 2>&1 &
server_pid=$!

if ! timeout 30 bash -c 'tail --pid="$1" -n +1 -F "$2" | grep -m1 -q "peryx listening"' _ \
  "$server_pid" "$work/server.log"; then
  status=running
  if ! kill -0 "$server_pid" 2>/dev/null; then
    set +e
    wait "$server_pid"
    status=$?
    set -e
  fi
  echo "peryx did not report a listening socket within 30s; process status: $status"
  cat "$work/server.log"
  exit 1
fi

if ! curl -sf "http://127.0.0.1:$port/v2/" >/dev/null; then
  echo "peryx reported its listener but the OCI endpoint failed"
  cat "$work/server.log"
  exit 1
fi

report="$work/conformance.log"
set +e
OCI_ROOT_URL="http://127.0.0.1:$port" \
  OCI_NAMESPACE=store/conformance \
  OCI_CROSSMOUNT_NAMESPACE=store/crossmount \
  OCI_USERNAME=_ \
  OCI_PASSWORD=conformance \
  OCI_TEST_PULL=1 OCI_TEST_PUSH=1 OCI_TEST_CONTENT_DISCOVERY=1 OCI_TEST_CONTENT_MANAGEMENT=1 \
  "$conformance" >"$report" 2>&1
set -e

required_failures=$(grep 'failed test' "$report" | grep -viE 'sha512' || true)
optional_failures=$(grep -c 'sha512.*failed test\|failed test.*sha512' "$report" || true)

if [ -n "$required_failures" ]; then
  echo "required (sha256) OCI conformance tests failed"
  echo "$required_failures"
  exit 1
fi

echo "all required (sha256) OCI conformance tests passed"
echo "$optional_failures optional sha512 tests failed (peryx stores sha256 blobs only)"
