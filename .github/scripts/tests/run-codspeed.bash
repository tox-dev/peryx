#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/work"
jq -n --arg root "$scratch/work" '{packages: [{
  name: "owner", manifest_path: ($root + "/Cargo.toml"), metadata: {"peryx-ci": {codspeed: {
    label: "Owner", "change-key": "owner", order: 0, "cargo-jobs": 2, benches: ["fixture"]
  }}}
}]}' >"$scratch/metadata.json"

cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo\t%s\n' "$*" >>"$COMMAND_LOG"
if [[ $1 == metadata ]]; then
  cat "$CARGO_METADATA_FIXTURE"
elif [[ $1 == codspeed && $2 == build ]]; then
  mkdir -p target/codspeed/analysis/owner
  printf 'benchmark\n' >target/codspeed/analysis/owner/fixture
fi
EOF
cat >"$scratch/bin/codspeed" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'codspeed\t%s\n' "$*" >>"$COMMAND_LOG"
EOF
ln -s cargo "$scratch/bin/cargo-codspeed"
cat >"$scratch/bin/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
:
EOF
cat >"$scratch/bin/perl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
seconds=$3
shift 3
printf 'deadline\t%s\t%s\n' "$seconds" "$*" >>"$COMMAND_LOG"
if [[ ${TIME_OUT_SECONDS:-} == "$seconds" ]]; then
  exit 142
fi
exec "$@"
EOF
chmod +x "$scratch/bin/"*
export CARGO_METADATA_FIXTURE="$scratch/metadata.json"
export COMMAND_LOG="$scratch/commands.log"
export PATH="$scratch/bin:$PATH"

(cd "$scratch/work" && CODSPEED_SKIP_UPLOAD=true "$repo/ci/run-codspeed.sh" owner 2 >/dev/null)
cat >"$scratch/expected.log" <<'EOF'
cargo	metadata --no-deps --format-version 1
deadline	1200	cargo codspeed build --locked -j 2 -m simulation -p owner --bench fixture
cargo	codspeed build --locked -j 2 -m simulation -p owner --bench fixture
deadline	600	codspeed run --mode simulation --skip-upload -- cargo codspeed run -p owner --bench fixture
codspeed	run --mode simulation --skip-upload -- cargo codspeed run -p owner --bench fixture
EOF
cmp "$scratch/expected.log" "$COMMAND_LOG"

if output=$(cd "$scratch/work" && TIME_OUT_SECONDS=1 CODSPEED_BUILD_TIMEOUT_SECONDS=1 \
  "$repo/ci/run-codspeed.sh" owner 2 2>&1); then
  printf 'timed-out CodSpeed build passed\n' >&2
  exit 1
fi
grep -Fq 'command timed out after 1 seconds: cargo codspeed build' <<<"$output"

if output=$(cd "$scratch/work" && CODSPEED_RUN_TIMEOUT_SECONDS=none \
  "$repo/ci/run-codspeed.sh" owner 2 2>&1); then
  printf 'invalid CodSpeed deadline passed\n' >&2
  exit 1
fi
[[ $output == 'CodSpeed process deadlines must be positive integers' ]]
