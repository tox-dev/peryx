#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
cd "$repo"
if rg -n 'TWINE_SPEC|uv tool install.*twine|pipx:twine' .github/workflows scripts/ci/Dockerfile mise.toml; then
  printf 'shared automation installs an owner client\n' >&2
  exit 1
fi
if rg -n -- '--partition|partition:' .github/workflows/ci.yml; then
  printf 'platform CI uses hash partitions\n' >&2
  exit 1
fi
[[ $(rg -c 'just platform-contract' .github/workflows/ci.yml) == 1 ]]
rg -q 'os: \[macos-26, windows-2025\]' .github/workflows/ci.yml
rg -q 'just fuzz-package peryx-ecosystem-pypi 30' .github/workflows/ci.yml
rg -q 'just fuzz-package peryx-ecosystem-oci 30' .github/workflows/ci.yml
sed -n '/^  frontend:/,/^  coverage:/p' .github/workflows/ci.yml | grep -Fq 'sudo apt-get install --yes lcov'
rg -Fq -- "- 'crates/*/docs/**'" .github/workflows/ci.yml
rg -Fq -- "- '!crates/*/docs/**'" .github/workflows/ci.yml
codspeed_shared=$(sed -n '/^            shared:/,/^            runner:/p' \
  .github/workflows/codspeed.yml)
if grep -Fxq "              - 'crates/**'" <<<"$codspeed_shared"; then
  printf 'CodSpeed shared changes include every owner path\n' >&2
  exit 1
fi
for pattern in \
  "crates/peryx-archive/{Cargo.toml,build.rs,src/*,src/!(tests)/**,benches/**}" \
  "crates/peryx-core/{Cargo.toml,build.rs,src/*,src/!(tests)/**,benches/**}" \
  "crates/peryx-ha/{Cargo.toml,build.rs,src/*,src/!(tests)/**,benches/**}" \
  "crates/peryx-ha-distributed/{Cargo.toml,build.rs,src/*,src/!(tests)/**,benches/**}" \
  "crates/peryx-pql/{Cargo.toml,build.rs,src/*,src/!(tests)/**,benches/**}" \
  "crates/peryx-ecosystem-pypi/{Cargo.toml,build.rs,src/*,src/!(tests)/**,benches/**}" \
  "crates/peryx-ecosystem-oci/{Cargo.toml,build.rs,src/*,src/!(tests)/**,benches/**}"; do
  grep -Fq -- "- '$pattern'" .github/workflows/codspeed.yml
done
metadata=$(cargo metadata --no-deps --format-version 1)
workspace_crates=$(jq -r '.packages[].manifest_path | sub("/Cargo.toml$"; "")' <<<"$metadata")
while IFS= read -r crate; do
  if ! grep -Fxq "$repo/$crate" <<<"$workspace_crates"; then
    printf 'CodSpeed filter names a missing workspace crate: %s\n' "$crate" >&2
    exit 1
  fi
done < <(rg -o "crates/[^/{']+" .github/workflows/codspeed.yml | sort -u)
rg -q 'timings-\$SHARD[.]jsonl' .github/workflows/ci.yml
rg -q 'just conformance peryx-ecosystem-oci' .github/workflows/conformance.yml
if rg -n 'DISTRIBUTION_SPEC_REF|fcfba1ec' .github/workflows; then
  printf 'workflow contains an owner conformance revision\n' >&2
  exit 1
fi
[[ $(find . -maxdepth 2 -type f -name 'compose*.yaml' -print | sort) == ./compose.yaml ]]
while IFS= read -r target; do
  if rg -Fn "$target" .github/workflows; then
    printf 'workflow contains owner fuzz target: %s\n' "$target" >&2
    exit 1
  fi
done < <(jq -r '.packages[].metadata["peryx-ci"].fuzz.targets[]?' <<<"$metadata")
jq -e '
  all(.packages[] | select(.metadata["peryx-ci"].codspeed != null);
    (.metadata["peryx-ci"].codspeed.benches | type) == "array")
  and all(.packages[] | select(.metadata["peryx-ci"].fuzz != null);
    (.metadata["peryx-ci"].fuzz.targets | length) > 0)
  and all(.packages[] | select(.metadata["peryx-ci"].conformance != null);
    (.metadata["peryx-ci"].conformance.revision | length) == 40
    and (.metadata["peryx-ci"].conformance.runner | length) > 0)
' <<<"$metadata" >/dev/null
