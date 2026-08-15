#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/owner/tools"
cat >"$scratch/metadata.json" <<EOF
{"packages":[{"name":"owner","manifest_path":"$scratch/owner/Cargo.toml","metadata":{"peryx-ci":{
  "tools":{"install":"tools/install"}}}}]}
EOF
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
if [[ $* == 'metadata --no-deps --format-version 1' || $* == metadata\ --manifest-path* ]]; then
  cat "$METADATA"
else
  printf 'cargo|%s\n' "$*" >>"$CALL_LOG"
fi
EOF
cat >"$scratch/owner/tools/install" <<'EOF'
#!/usr/bin/env bash
printf 'install|%s\n' "$1" >>"$CALL_LOG"
EOF
chmod +x "$scratch/bin/cargo" "$scratch/owner/tools/install"
export CALL_LOG="$scratch/calls" METADATA="$scratch/metadata.json"
PATH="$scratch/bin:$PATH" "$repo/scripts/ci/package-tools" owner
grep -Fqx "install|$repo/.tox/tools" "$CALL_LOG"
