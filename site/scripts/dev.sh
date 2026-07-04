#!/usr/bin/env bash
# Zola's dev server with live reload AND a Pagefind search index that refreshes on file changes.
#
# `zola serve` has no post-build hook and serves from memory, so it can't run Pagefind itself. But
# Zola serves site/static/ verbatim, so the search bundle lives at static/pagefind and `zola serve`
# picks it up on the fly. A background loop rebuilds the index whenever content, templates, or
# styles change, so the ⌘K search works on the dev server just as it does in production. The bundle
# is a build artifact (gitignored). Install watchexec for lower-latency reindexing than the poll.
set -euo pipefail
cd "$(dirname "$0")/.." # -> site/

run_pagefind() { if command -v pagefind >/dev/null; then pagefind "$@"; else npx -y pagefind@latest "$@"; fi; }

reindex() {
  out="$(mktemp -d)"
  if zola build -o "$out" --base-url "http://127.0.0.1:1111" --force >/dev/null 2>&1; then
    run_pagefind --site "$out" --output-path static/pagefind --include-characters "_-./" >/dev/null 2>&1 || true
  fi
  rm -rf "$out"
}

reindex # prime the index before the first request

watch() {
  stamp="$(mktemp)"
  while sleep 3; do
    if find content templates sass config.toml -type f -newer "$stamp" 2>/dev/null | grep -q .; then
      touch "$stamp"
      reindex
    fi
  done
}
watch &
loop=$!

zola --root . serve --interface 127.0.0.1 &
server=$!
trap 'kill "$loop" "$server" 2>/dev/null || true' EXIT INT TERM
wait "$server"
