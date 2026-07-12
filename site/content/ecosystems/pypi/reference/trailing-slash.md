+++
title = "Trailing-slash redirects"
description = "The exact rule: a Simple index or project URL without the trailing slash returns 301 to the PEP 503-normalized slashed URL, query string preserved; a project segment with a slash is not redirected."
weight = 3
+++

The [Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/) canonical URLs end in a
slash. A request that drops the slash on the index or a project is redirected to the slashed form rather than answered
with a `404`. `{route}` below is the index's route, for example `root/pypi`.

## Rule

| Request                          | Response | `Location`                      |
| -------------------------------- | -------- | ------------------------------- |
| `GET /{route}/simple`            | `301`    | `/{route}/simple/`              |
| `GET /{route}/simple/{project}`  | `301`    | `/{route}/simple/{normalized}/` |
| `GET /{route}/simple/`           | `200`    | served directly, not redirected |
| `GET /{route}/simple/{project}/` | `200`    | served directly, not redirected |

The status is `301 Moved Permanently`, the same status pypi.org (Warehouse) returns. `{normalized}` is `{project}` after
[PEP 503](https://peps.python.org/pep-0503/) normalization.

## Details

- **Normalization.** The project segment in the `Location` is normalized: lowercased, with every run of `.`, `-`, or `_`
  collapsed to a single `-`. `Flask.Test` redirects to `/{route}/simple/flask-test/`. An already-canonical name
  redirects to itself with the slash appended.
- **Query string.** Any query string on the request is preserved on the `Location` unchanged.
  `GET /{route}/simple/Flask.Test?extra=1` redirects to `/{route}/simple/flask-test/?extra=1`.
- **Location form.** The `Location` is a path (host-absolute), built from the request path with the route prefix intact,
  so the redirect stays on the same origin and works behind a proxy or under a nested route.
- **A project segment with a slash is not redirected.** The redirect fires only for a single segment after `simple/`. A
  path with a further slash, such as `/{route}/simple/some/thing`, is not a project name, is not redirected, and falls
  through to a `404`.
- **Already-slashed URLs are served, not redirected.** `/{route}/simple/` and `/{route}/simple/{project}/` are the
  canonical URLs; they return their content directly. Content negotiation, policy, and caching apply as normal.
- **Method.** The redirect is defined for `GET` on these two Simple read paths. It does not change the upload, yank,
  delete, files, inspect, or legacy JSON routes.
