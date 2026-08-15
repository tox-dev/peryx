#!/usr/bin/env bash
set -euo pipefail

: "${READTHEDOCS_CANONICAL_URL:?}"
: "${READTHEDOCS_OUTPUT:?}"

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
tools="$repo/.tox/readthedocs-tools"
mkdir -p "$tools"
install_release_tool() {
  local repository=$1 name=$2 target=$3 tag
  tag=$(curl --fail --silent --location "https://api.github.com/repos/$repository/releases/latest" |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["tag_name"])')
  curl --fail --silent --location \
    "https://github.com/$repository/releases/download/$tag/$name-$tag-$target.tar.gz" |
    tar xz -C "$tools"
}
install_release_tool getzola/zola zola x86_64-unknown-linux-gnu
install_release_tool CloudCannon/pagefind pagefind x86_64-unknown-linux-musl
export PATH="$tools:$PATH"

"$repo/site/scripts/stage.sh"
mkdir -p "$repo/.tox/site/static"
cargo run --quiet --manifest-path "$repo/Cargo.toml" --package peryx --bin peryx -- openapi \
  >"$repo/.tox/site/static/openapi.json"

zola --root "$repo/.tox/site" build --base-url "$READTHEDOCS_CANONICAL_URL" --force
python3 "$repo/.tox/site/scripts/inline_diagrams.py" "$repo/.tox/site/public"
pagefind --site "$repo/.tox/site/public" --include-characters "_./-"
python3 "$repo/.tox/site/scripts/gen_llms.py" \
  --base-url "$READTHEDOCS_CANONICAL_URL" \
  --content "$repo/.tox/site/content" \
  --out "$repo/.tox/site/public/llms.txt"
mkdir -p "$READTHEDOCS_OUTPUT/html"
cp -R "$repo/.tox/site/public/." "$READTHEDOCS_OUTPUT/html/"
