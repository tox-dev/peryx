+++
title = "Use the web UI"
description = "Browse indexes, read package pages, inspect status, and inspect archive contents from the browser."
weight = 7
+++

velodex serves a reactive web interface on its own port: server-rendered pages that hydrate in the browser, in the same
visual style as this site.

## Dashboard

`http://<host>:<port>/` shows the version, the change serial, live request counters (including PEP 658 metadata hits),
and a card per configured index with its kind, route, layers, whether it accepts uploads, and its usage: pages served,
downloads, and bytes. The counters refresh every few seconds.

{{ screen(alt="The dashboard: counters on top, one card per index with its layer stack and usage", name="dashboard") }}

Each card's `usage` link opens the drill-down described in [monitoring](@/guides/monitor.md): index totals, a
per-project table, and per-file download counts.

## Admin status

`/admin/status` reads `GET /+status?details=admin` and top-level `GET /+stats`. It shows configured repositories,
routes, overlay member order, upload targets by name, observed project counts, uploaded file counts, recent uploads,
mirror URLs, redacted authentication state, and cache-health counters. It also links to the JSON status, JSON stats,
Prometheus metrics, Simple API, browse, and usage pages.

The admin status document scans metadata keys once to count observed projects and uploaded files, then keeps only a
capped recent-upload list per index. It does not fetch upstreams, read package detail pages, read artifacts, or expose
upload tokens, upstream passwords, bearer tokens, URL user info, URL queries, or URL fragments.

## Browsing packages

An index card links to its project list, filterable as you type. A project page shows what pypi.org would: the rendered
long description, summary, install command with a copy button, versions, dependencies, keywords, license, author,
project links, grouped classifiers, and a file table with sizes, upload dates, sha256 digests, and yank/metadata badges.

{{ screen(alt="A project page: description and files on the left, metadata panel on the right", name="project") }}

Inspectable wheels, zips, and `.tar.gz` sdists get a `contents` link. It opens the archive browser: members with their
sizes, and member text in bounded chunks for large generated files. Unsupported formats still show as downloadable
files, but do not get a broken archive link. The browser URL stores the file's sha256, display filename, selected
member, and chunk offset as separate query parameters. That keeps links stable for filenames and member paths containing
spaces, slashes, `#`, or `?`.

## Managing uploads

"Manage uploads" on a project page takes the index's upload token and offers yank, un-yank, and delete per version, plus
whole-project delete. The buttons drive the same HTTP endpoints as [curl would](@/guides/remove.md), so the rules match:
deleting uploads needs a `volatile` index, and files served from a mirror are hidden reversibly rather than deleted.

## Requirements

The interactive layer is a wasm bundle built by [cargo-leptos](https://github.com/leptos-rs/cargo-leptos)
(`cargo leptos build --release`, output in `ui/pkg/`, served at `/pkg`). Without the bundle every page still renders
server-side; filtering, live counters, and the admin buttons need it.

## Related

- The endpoints the UI reads: [HTTP endpoints](@/reference/endpoints.md)
- Operational counters and status data: [monitoring](@/guides/monitor.md)
- How the UI is built and tested: [architecture](@/explanation/architecture.md)
