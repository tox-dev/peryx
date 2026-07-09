#!/usr/bin/env bash
# Run the OCI distribution-spec conformance suite against a hosted velodex registry and gate on the
# required (sha256) tests. sha512 is the optional digest algorithm velodex does not store, so its
# failures are reported but not fatal.
#
# Usage: run-oci-conformance.sh <velodex-binary> <conformance.test-binary>
set -euo pipefail

velodex=${1:?path to the velodex binary}
conformance=${2:?path to the conformance.test binary}

port=18102
work=$(mktemp -d)
trap 'kill "${server_pid:-0}" 2>/dev/null || true; rm -rf "$work"' EXIT

cat >"$work/velodex.toml" <<EOF
host = "127.0.0.1"
port = $port
data_dir = "$work/data"

[[index]]
name = "store"
route = "store"
ecosystem = "oci"
hosted = true
upload_token = "conformance"
EOF

"$velodex" serve --config "$work/velodex.toml" >"$work/server.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 60); do
  if curl -sf "http://127.0.0.1:$port/v2/" >/dev/null 2>&1; then break; fi
  sleep 0.5
done

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

# Every failing test line names the workflow; the required suite is everything that is not sha512.
required_failures=$(grep 'failed test' "$report" | grep -viE 'sha512' || true)
optional_failures=$(grep -c 'sha512.*failed test\|failed test.*sha512' "$report" || true)

if [ -n "$required_failures" ]; then
  echo "FAIL: required (sha256) OCI conformance tests failed:"
  echo "$required_failures"
  exit 1
fi

echo "PASS: all required (sha256) OCI conformance tests passed."
echo "note: $optional_failures optional sha512 tests failed (velodex stores sha256 blobs only)."
