#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
scratch=$(cd "$scratch" && pwd -P)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/packages/owner/src" "$scratch/packages/shared" \
  "$scratch/packages/shared/nested/src" "$scratch/packages/shared/standalone/src" \
  "$scratch/packages/shared/workspace/members/one/src" \
  "$scratch/packages/shared/workspace/members/two/src" "$scratch/compiler"
printf 'pub fn owner() {}\n' >"$scratch/packages/owner/src/lib.rs"
printf 'pub fn shared() {}\n' >"$scratch/packages/shared/shared.rs"
printf 'pub fn nested() {}\n' >"$scratch/packages/shared/nested/src/lib.rs"
printf 'pub fn standalone() {}\n' >"$scratch/packages/shared/standalone/src/lib.rs"
printf 'pub fn workspace_one() {}\n' \
  >"$scratch/packages/shared/workspace/members/one/src/lib.rs"
printf 'pub fn workspace_two() {}\n' \
  >"$scratch/packages/shared/workspace/members/two/src/lib.rs"
printf 'fixture_macro!();\n' >"$scratch/compiler/macro_only.rs"
printf 'pub mod child;\n' >"$scratch/compiler/empty_root.rs"
printf '[package]\nname = "owner"\nversion = "0.0.0"\n' >"$scratch/packages/owner/Cargo.toml"
printf '[package]\nname = "nested"\nversion = "0.0.0"\n' >"$scratch/packages/shared/nested/Cargo.toml"
printf '[package]\nname = "standalone"\nversion = "0.0.0"\n' \
  >"$scratch/packages/shared/standalone/Cargo.toml"
printf '[workspace]\nmembers = ["members/*"]\n' >"$scratch/packages/shared/workspace/Cargo.toml"
printf '[package]\nname = "workspace-one"\nversion = "0.0.0"\n' \
  >"$scratch/packages/shared/workspace/members/one/Cargo.toml"
printf '[package]\nname = "workspace-two"\nversion = "0.0.0"\n' \
  >"$scratch/packages/shared/workspace/members/two/Cargo.toml"
jq -n --arg root "$scratch/packages" '{packages: [
  {name: "owner", manifest_path: ($root + "/owner/Cargo.toml"), metadata: {"peryx-ci": {"coverage-source-roots": ["../shared"], fuzz: {manifest: "../shared/standalone/Cargo.toml", targets: ["standalone_contract"]}}}, targets: [{src_path: $compiler}, {src_path: $empty}]},
  {name: "nested", manifest_path: ($root + "/shared/nested/Cargo.toml"), metadata: {}, targets: [{src_path: ($root + "/shared/nested/src/lib.rs")}]}
]}' --arg compiler "$scratch/compiler/macro_only.rs" --arg empty "$scratch/compiler/empty_root.rs" \
  >"$scratch/metadata.json"
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$CARGO_LOG"
if [[ $1 == metadata ]]; then
  cat "$CARGO_METADATA_FIXTURE"
fi
EOF
chmod +x "$scratch/bin/cargo"
export CARGO_LOG="$scratch/cargo.log"
export CARGO_METADATA_FIXTURE="$scratch/metadata.json"
export PATH="$scratch/bin:$PATH"

cd "$repo"
expected=$(printf '%s\n' "$scratch/packages/owner" "$scratch/packages/shared")
[[ $(.github/scripts/package-source-roots --metadata "$scratch/metadata.json" owner) == "$expected" ]]
.github/scripts/package-source-roots --metadata "$scratch/metadata.json" --index >"$scratch/roots"
grep -Fq $'owner\t'"$scratch/packages/owner" "$scratch/roots"
grep -Fq $'nested\t'"$scratch/packages/shared/nested" "$scratch/roots"
grep -Fxq $'\t'"$scratch/packages/shared/standalone" "$scratch/roots"
grep -Fxq $'\t'"$scratch/packages/shared/workspace" "$scratch/roots"
grep -Fxq $'\t'"$scratch/packages/shared/workspace/members/one" "$scratch/roots"
grep -Fxq $'\t'"$scratch/packages/shared/workspace/members/two" "$scratch/roots"
if .github/scripts/package-source-roots --metadata "$scratch/metadata.json" absent; then
  printf 'unknown source package passed\n' >&2
  exit 1
fi
if .github/scripts/package-source-roots --metadata "$scratch/metadata.json" --index owner; then
  printf 'index accepted a package\n' >&2
  exit 1
fi
if .github/scripts/package-source-roots --metadata "$scratch/metadata.json"; then
  printf 'missing source package passed\n' >&2
  exit 1
fi
: >"$CARGO_LOG"
.github/scripts/package-source-roots owner >/dev/null
[[ $(wc -l <"$CARGO_LOG" | tr -d ' ') == 1 ]]
.github/scripts/package-contract-inputs "$scratch/metadata.json" "$scratch/roots" owner >"$scratch/inputs"
grep -Fxq "$scratch/compiler/macro_only.rs" "$scratch/inputs"
if grep -Fq "$scratch/packages/shared/standalone/src/lib.rs" "$scratch/inputs"; then
  printf 'standalone package entered parent contract inputs\n' >&2
  exit 1
fi
if grep -Fq "$scratch/packages/shared/workspace/members/one/src/lib.rs" "$scratch/inputs"; then
  printf 'workspace package entered parent contract inputs\n' >&2
  exit 1
fi

{
  printf 'SF:%s\nFN:1,1,owner\nFNDA:1,owner\nDA:1,1\nend_of_record\n' \
    "$scratch/packages/owner/src/lib.rs"
  printf 'SF:%s\nFN:1,1,shared\nFNDA:1,shared\nDA:1,1\nend_of_record\n' \
    "$scratch/packages/shared/shared.rs"
  printf 'SF:%s\nend_of_record\n' "$scratch/compiler/macro_only.rs"
  printf 'SF:%s\nend_of_record\n' "$scratch/compiler/empty_root.rs"
} >"$scratch/owner.lcov"
.github/scripts/check-lcov-sources --metadata "$scratch/metadata.json" \
  --root-index "$scratch/roots" "$scratch/owner.lcov" owner >/dev/null
.github/scripts/check-lcov-sources --metadata "$scratch/metadata.json" "$scratch/owner.lcov" owner >/dev/null
awk -v omitted="SF:$scratch/packages/owner/src/lib.rs" '
  /^SF:/ { emit = $0 != omitted }
  emit { print }
' "$scratch/owner.lcov" >"$scratch/without-parent-source.lcov"
if output=$(.github/scripts/check-lcov-sources --metadata "$scratch/metadata.json" \
  --root-index "$scratch/roots" "$scratch/without-parent-source.lcov" owner 2>&1); then
  printf 'missing parent source passed\n' >&2
  exit 1
fi
grep -Fq "$scratch/packages/owner/src/lib.rs" <<<"$output"
awk -v omitted="SF:$scratch/compiler/macro_only.rs" '
  /^SF:/ { emit = $0 != omitted }
  emit { print }
' "$scratch/owner.lcov" >"$scratch/without-compiler-input.lcov"
if output=$(.github/scripts/check-lcov-sources --metadata "$scratch/metadata.json" \
  --root-index "$scratch/roots" "$scratch/without-compiler-input.lcov" owner 2>&1); then
  printf 'missing compiler input passed\n' >&2
  exit 1
fi
grep -Fq "$scratch/compiler/macro_only.rs" <<<"$output"
awk -v omitted="SF:$scratch/compiler/empty_root.rs" '
  /^SF:/ { emit = $0 != omitted }
  emit { print }
' "$scratch/owner.lcov" >"$scratch/without-zero-source.lcov"
.github/scripts/check-lcov-sources --metadata "$scratch/metadata.json" \
  --root-index "$scratch/roots" "$scratch/without-zero-source.lcov" owner >/dev/null
: >"$CARGO_LOG"
.github/scripts/check-lcov-sources --root-index "$scratch/roots" "$scratch/owner.lcov" owner >/dev/null
[[ $(wc -l <"$CARGO_LOG" | tr -d ' ') == 1 ]]

jq -n --arg root "$scratch/packages/shared/standalone" '{packages: [
  {name: "standalone", manifest_path: ($root + "/Cargo.toml"), metadata: {}, targets: [{src_path: ($root + "/src/lib.rs")}]}
]}' >"$scratch/standalone-metadata.json"
.github/scripts/package-source-roots --metadata "$scratch/standalone-metadata.json" --index \
  >"$scratch/standalone-roots"
printf 'SF:%s\nFN:1,1,standalone\nFNDA:1,standalone\nDA:1,1\nend_of_record\n' \
  "$scratch/packages/shared/standalone/src/lib.rs" >"$scratch/standalone.lcov"
.github/scripts/check-lcov-sources --metadata "$scratch/standalone-metadata.json" \
  --root-index "$scratch/standalone-roots" "$scratch/standalone.lcov" standalone >/dev/null
printf 'SF:%s\nend_of_record\n' "$scratch/packages/owner/src/lib.rs" \
  >"$scratch/standalone-missing.lcov"
if output=$(.github/scripts/check-lcov-sources --metadata "$scratch/standalone-metadata.json" \
  --root-index "$scratch/standalone-roots" "$scratch/standalone-missing.lcov" standalone 2>&1); then
  printf 'missing standalone source passed its own inventory\n' >&2
  exit 1
fi
grep -Fq "$scratch/packages/shared/standalone/src/lib.rs" <<<"$output"

jq -n --arg root "$scratch/packages/shared/workspace" '{packages: [
  {name: "workspace-one", manifest_path: ($root + "/members/one/Cargo.toml"), metadata: {}, targets: [{src_path: ($root + "/members/one/src/lib.rs")}]},
  {name: "workspace-two", manifest_path: ($root + "/members/two/Cargo.toml"), metadata: {}, targets: [{src_path: ($root + "/members/two/src/lib.rs")}]}
]}' >"$scratch/workspace-metadata.json"
.github/scripts/package-source-roots --metadata "$scratch/workspace-metadata.json" --index \
  >"$scratch/workspace-roots"
{
  printf 'SF:%s\nFN:1,1,workspace_one\nFNDA:1,workspace_one\nDA:1,1\nend_of_record\n' \
    "$scratch/packages/shared/workspace/members/one/src/lib.rs"
  printf 'SF:%s\nFN:1,1,workspace_two\nFNDA:1,workspace_two\nDA:1,1\nend_of_record\n' \
    "$scratch/packages/shared/workspace/members/two/src/lib.rs"
} >"$scratch/workspace.lcov"
.github/scripts/check-lcov-sources --metadata "$scratch/workspace-metadata.json" \
  --root-index "$scratch/workspace-roots" "$scratch/workspace.lcov" workspace-one workspace-two \
  >/dev/null
tail -5 "$scratch/workspace.lcov" >"$scratch/workspace-one-missing.lcov"
if output=$(.github/scripts/check-lcov-sources --metadata "$scratch/workspace-metadata.json" \
  --root-index "$scratch/workspace-roots" "$scratch/workspace-one-missing.lcov" workspace-one 2>&1); then
  printf 'missing workspace package source passed its own inventory\n' >&2
  exit 1
fi
grep -Fq "$scratch/packages/shared/workspace/members/one/src/lib.rs" <<<"$output"

head -5 "$scratch/owner.lcov" >"$scratch/missing.lcov"
if .github/scripts/check-lcov-sources --metadata "$scratch/metadata.json" \
  --root-index "$scratch/roots" "$scratch/missing.lcov" owner; then
  printf 'missing source passed\n' >&2
  exit 1
fi
if .github/scripts/check-lcov-sources --metadata "$scratch/metadata.json" \
  --root-index "$scratch/roots" "$scratch/owner.lcov" absent; then
  printf 'unknown inventory package passed\n' >&2
  exit 1
fi
: >"$scratch/empty.lcov"
if .github/scripts/check-lcov-sources "$scratch/empty.lcov" owner; then
  printf 'empty LCOV report passed\n' >&2
  exit 1
fi
printf 'SF:%s\nend_of_record\n' "$scratch/packages/owner/src/lib.rs" >"$scratch/bare.lcov"
.github/scripts/check-lcov-sources --metadata "$scratch/metadata.json" \
  --root-index "$scratch/roots" "$scratch/owner.lcov" owner >/dev/null
printf 'SF:%s\nDA:1,1\n' "$scratch/packages/owner/src/lib.rs" >"$scratch/unterminated.lcov"
if .github/scripts/check-lcov-sources "$scratch/unterminated.lcov" owner; then
  printf 'unterminated LCOV source record passed\n' >&2
  exit 1
fi

printf '#[component]\npub fn ArtifactPlacements() {\n    render();\n}\n' >"$scratch/component.rs"
printf 'SF:%s\nFN:3,_RN_component_artifact_placements\nFNDA:1,_RN_component_artifact_placements\nDA:3,1\nend_of_record\n' \
  "$scratch/component.rs" >"$scratch/component.lcov"
COVERAGE_ROOT="$scratch" COVERAGE_REQUIRE_EXECUTABLE=1 \
  .github/scripts/check-lcov-functions.awk "$scratch/component.lcov" >/dev/null
