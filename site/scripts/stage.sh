#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
stage=${1:-"$repo/.tox/site"}

copy_tree() {
  local source=$1
  local target=$2
  local destination path relative
  [[ -d $source ]] || return 0
  while IFS= read -r path; do
    relative=${path#"$source"/}
    destination=$target/$relative
    if [[ -d $path ]]; then
      if [[ -e $destination && ! -d $destination ]]; then
        printf 'owner docs path conflicts with staged file: %s\n' "$destination" >&2
        exit 1
      fi
    elif [[ -e $destination || -L $destination ]]; then
      printf 'owner docs file collision: %s\n' "$destination" >&2
      exit 1
    fi
  done < <(find "$source" -mindepth 1 -print | sort)
  mkdir -p "$target"
  cp -R "$source/." "$target/"
}

for generated in public static/openapi.json data/bench/report.toml data/ecosystems data/ecosystem-owners.toml \
  data/ecosystem-owner-links.md; do
  if [[ -e $repo/site/$generated ]]; then
    printf 'remove generated source-tree artifact before staging: site/%s\n' "$generated" >&2
    exit 1
  fi
done
rm -rf "$stage"
mkdir -p "$stage"
tar -C "$repo/site" \
  --exclude='./public' \
  --exclude='./data/bench' \
  --exclude='./node_modules' \
  -cf - . | tar -C "$stage" -xf -

mkdir -p "$stage/content/ecosystems" "$stage/data/ecosystems" "$stage/static" "$stage/templates"
printf '# Ecosystem owners declare these records.\n' >"$stage/data/ecosystem-owners.toml"
owner_links="$stage/data/ecosystem-owner-links.md"
: >"$owner_links"

while IFS= read -r manifest; do
  crate=${manifest%/Cargo.toml}
  package=$(sed -n 's/^name = "\(peryx-ecosystem-[a-z0-9-]*\)"$/\1/p' "$manifest" | head -1)
  owner=${package#peryx-ecosystem-}
  docs=$crate/docs
  declaration=$docs/ecosystem.toml
  if [[ -z $package || ! -f $declaration ]]; then
    printf 'owner crate lacks docs/ecosystem.toml: %s\n' "$crate" >&2
    exit 1
  fi
  declared_owner=$(sed -n 's/^[[:space:]]*id = "\([a-z0-9-]*\)".*/\1/p' "$declaration" | head -1)
  name=$(sed -n 's/^[[:space:]]*name = "\([^"]*\)".*/\1/p' "$declaration" | head -1)
  subtitle=$(sed -n 's/^[[:space:]]*subtitle = "\([^"]*\)".*/\1/p' "$declaration" | head -1)
  color=$(sed -n 's/^[[:space:]]*color = "\([^"]*\)".*/\1/p' "$declaration" | head -1)
  chip=$(sed -n -e 's/^[[:space:]]*chip = true.*/true/p' -e 's/^[[:space:]]*chip = false.*/false/p' "$declaration" | head -1)
  if [[ -z $declared_owner || -z $name || -z $subtitle || -z $color || -z $chip ]]; then
    printf 'owner metadata requires id, name, subtitle, color, and chip: %s\n' "$declaration" >&2
    exit 1
  fi
  if [[ $declared_owner != "$owner" ]]; then
    printf 'owner id %s must match crate %s\n' "$declared_owner" "$package" >&2
    exit 1
  fi
  if [[ $(awk '$0 == "[[items]]" { count++ } END { print count + 0 }' "$declaration") -ne 1 ]]; then
    printf 'owner metadata must contain one [[items]] record: %s\n' "$declaration" >&2
    exit 1
  fi
  if [[ ! -f $docs/content/_index.md ]]; then
    printf 'owner docs require content/_index.md: %s\n' "$crate" >&2
    exit 1
  fi
  if [[ -e $stage/content/ecosystems/$owner ]]; then
    printf 'owner route collision: /ecosystems/%s/\n' "$owner" >&2
    exit 1
  fi
  copy_tree "$docs/content" "$stage/content/ecosystems/$owner"
  copy_tree "$docs/data" "$stage/data/ecosystems/$owner"
  copy_tree "$docs/static" "$stage/static"
  copy_tree "$docs/templates" "$stage/templates"
  cat "$declaration" >>"$stage/data/ecosystem-owners.toml"
  printf '\n' >>"$stage/data/ecosystem-owners.toml"
  printf -- '- [%s](/ecosystems/%s/): %s\n' "$name" "$owner" "$subtitle" >>"$owner_links"
done < <(find "$repo/crates" -mindepth 2 -maxdepth 2 -type f -path '*/peryx-ecosystem-*/Cargo.toml' | sort)

node "$stage/scripts/migrate_content.mjs" "$stage/content" "$owner_links"
