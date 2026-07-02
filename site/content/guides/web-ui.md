+++
title = "Use the web UI"
description = "Browse indexes, read package pages, inspect archive contents, and manage uploads from the browser."
weight = 5
+++

velodex serves a reactive web interface on its own port: server-rendered pages that hydrate in the browser, in the same
visual style as this site.

## Dashboard

`http://<host>:<port>/` shows the version, the change serial, live request counters (including PEP 658 metadata hits),
and a card per configured index with its kind, route, layers, whether it accepts uploads, and its usage: pages served,
downloads, and bytes. The counters refresh every few seconds. Each card's `usage` link opens the drill-down described
in [monitoring](@/guides/monitor.md): index totals, a per-project table, and per-file download counts.

## Browsing packages

An index card links to its project list, filterable as you type. A project page shows what pypi.org would: the rendered
long description, summary, install command with a copy button, versions, dependencies, keywords, license, author,
project links, grouped classifiers, and a file table with sizes, upload dates, sha256 digests, and yank/metadata badges.

Each file's `contents` link opens the archive browser: the members of the wheel or sdist with their sizes, and each
text member readable in place, the way [pypi-browser](https://github.com/chriskuehl/pypi-browser) presents packages.
Members over 1 MiB are not shown inline; download the artifact instead.

## Managing uploads

"Manage uploads" on a project page takes the index's upload token and offers yank, un-yank, and delete per version,
plus whole-project delete. The buttons drive the same HTTP endpoints as [curl would](@/guides/remove.md), so the rules
match: deleting uploads needs a `volatile` index, and files served from a mirror are hidden reversibly rather than
deleted.

## Requirements

The interactive layer is a wasm bundle built by [cargo-leptos](https://github.com/leptos-rs/cargo-leptos)
(`cargo leptos build --release`, output in `ui/pkg/`, served at `/pkg`). Without the bundle every page still renders
server-side; filtering, live counters, and the admin buttons need it.
