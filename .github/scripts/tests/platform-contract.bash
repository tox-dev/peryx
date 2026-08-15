#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/repo/scripts/ci"
cp "$repo/scripts/ci/platform-contract" "$scratch/repo/scripts/ci/"
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$CALL_LOG"
EOF
chmod +x "$scratch/bin/cargo" "$scratch/repo/scripts/ci/platform-contract"
export CALL_LOG="$scratch/calls"
PATH="$scratch/bin:$PATH" "$scratch/repo/scripts/ci/platform-contract"
(("$(wc -l <"$CALL_LOG")" == 2))
grep -Fqx 'check --workspace --all-targets --all-features' "$CALL_LOG"
grep -Fq 'nextest run --workspace --all-features -E ' "$CALL_LOG"
grep -Fq 'binary(cli_entrypoint)' "$CALL_LOG"
grep -Fq 'binary(tls)' "$CALL_LOG"
grep -Fq 'test(/client::(exec|credential|netrc)/)' "$CALL_LOG"
grep -Fq 'test(/process/)' "$CALL_LOG"
grep -Fq 'test(/blob_backend/)' "$CALL_LOG"
if grep -Fq -- '--partition' "$CALL_LOG"; then
  printf 'platform contract uses test partitioning\n' >&2
  exit 1
fi
