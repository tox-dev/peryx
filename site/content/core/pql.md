+++
title = "Query language (PQL)"
description = "Run read-only structured queries over peryx's operational domains through one typed endpoint."
weight = 8
+++

PQL, the Peryx Query Language, is one read-only query surface over peryx's operational state. Instead of learning a
separate endpoint, cursor, and permission rule for every read, you send a small textual query to `POST /+query` and get
back a bounded page of typed rows. The language selects, filters, orders, aggregates, and pages; it never writes,
deletes, or mutates anything, and it cannot be extended to.

PQL is structured selection over typed metadata, not full-text search. For ranked text search over cached packages, use
`/+search`; PQL answers precise questions like "denied policy decisions for this repository, newest first" that a fixed
endpoint may not anticipate.

## The query shape

A query names one domain and then narrows it:

```
from <domain>
[ where <predicate> ]
[ select <field> [, <field> ...] ]
[ aggregate <func>(<column>) as <alias> [, ...] by <key> [, ...] ]
[ order by <field> [asc|desc] [, ...] ]
[ limit <n> ]
```

The `where` predicate is deliberately small: `and`, `or`, `not`, parentheses, the comparisons `==` `!=` `<` `<=` `>`
`>=`, membership with `in (...)`, and prefix match with `starts_with`. There is no arithmetic, no function call, and no
leading-wildcard match, so a query's cost is bounded before it runs. Literals are strings (`"deny"`), integers (`42`),
booleans (`true`), and timestamps written as `@` followed by an RFC 3339 instant (`@2026-06-01T00:00:00Z`).

Values bind out of band. A `:name` placeholder in the query is filled from the `params` object, never spliced into the
text, so a caller value can never change what a query means:

```console
curl -u alice:$PASSWORD \
  -H 'content-type: application/json' \
  -d '{
        "query": "from policy.decisions where repository == :repo and state == \"deny\" order by evaluated_at desc limit 25",
        "params": {"repo": "pypi-proxy"}
      }' \
  http://127.0.0.1:4433/+query
```

The response is a page of rows and an opaque cursor:

```json
{
  "rows": [
    {
      "repository": "pypi-proxy",
      "project": "requests",
      "state": "deny",
      "action": "serve",
      "evaluated_at": 1800000000,
      "fresh": true
    }
  ],
  "next_cursor": null
}
```

## Domains and fields

A domain is a typed relation. This release serves two: `policy.decisions`, the bounded history of index policy
evaluations, and `usage.downloads`, the durable per-project download and byte totals.

`policy.decisions` columns:

| Column                                         | Type      | Meaning                                                               |
| ---------------------------------------------- | --------- | --------------------------------------------------------------------- |
| `repository`, `project`, `version`, `filename` | string    | The package subject the policy evaluated                              |
| `action`                                       | string    | The operation evaluated, such as serving an artifact                  |
| `state`                                        | string    | `allow`, `deny`, or `wait`                                            |
| `evaluated_at`                                 | timestamp | When the evaluation ran, as whole Unix seconds                        |
| `fresh`                                        | bool      | Whether the current repository, catalog, and policy generations match |
| `source`, `rule`, `reason`                     | string    | The routed source, matched rule, and its explanation (operator-only)  |

`usage.downloads` columns:

| Column                  | Type   | Meaning                               |
| ----------------------- | ------ | ------------------------------------- |
| `repository`, `project` | string | The project the totals belong to      |
| `downloads`             | int    | Lifetime downloads served             |
| `bytes`                 | int    | Lifetime bytes served (operator-only) |

`select *` (or omitting `select`) returns every column the caller may read; naming columns returns just those. Results
order newest-first by `evaluated_at` (or, for usage, by `downloads`) unless you order otherwise.

Aggregation is capped at `count`, `sum`, `min`, and `max` over a declared numeric column, grouped by declared keys, for
example how many decisions each state produced:

```console
curl -u alice:$PASSWORD -H 'content-type: application/json' \
  -d '{"query": "from policy.decisions aggregate count() as decisions by state"}' \
  http://127.0.0.1:4433/+query
```

Time-bucketed windowing is not part of the language; the `/+analytics/timeline` endpoint keeps serving that.

## Joining two domains

One query may correlate two domains through a bounded, declared join on their shared keys. The join is inner: an outer
row appears only when the joined domain has a matching row. Correlate policy decisions with download totals to find,
say, denied projects and how much traffic they still draw:

```console
curl -u alice:$PASSWORD -H 'content-type: application/json' \
  -d '{"query": "from policy.decisions join usage.downloads on repository, project where state == \"deny\" order by downloads desc limit 25"}' \
  http://127.0.0.1:4433/+query
```

A join is admitted only when the joined domain has an index on every join key, so each outer row is a bounded lookup
rather than a scan. A join whose key the joined domain cannot serve cheaply is refused with a `400`. Field visibility
still applies to both sides: a column present in both domains, or contributed by either, keeps the stricter
classification, so `usage.downloads.bytes` (operator-only) drops from a join a repository reader runs.

## Authorization and field visibility

You never write your own scope. The evaluator resolves your credential once and injects the repositories you may read as
a predicate the query can neither name nor remove, applied before ordering and paging so counts and pagination never
leak a row you cannot see.

Name a repository you can read by its configured repository name (`where repository == "pypi-proxy"`, the name grants
and records use, not the URL route) and the query scopes to it; a repository token or a repository reader reaches
exactly its own repository. Omit the repository and the query runs operator-wide behind a local administrator
credential. A caller who cannot read a domain receives a `404`, so a denial never confirms the domain exists.

Column visibility follows the same classification every peryx endpoint uses. A repository-scoped caller sees the
repository-level columns; the operator-only columns (`source`, `rule`, `reason`) are dropped from their rows. Because
those columns are operator-classified, any result that includes them is served `Cache-Control: no-store` and never
enters a shared cache; a purely repository-level result is served `private, no-cache`.

## Pagination

When more rows match than the page holds, the response carries a `next_cursor`. Send it back as the `cursor` field to
read the next page. The cursor is opaque and bound to the scope that minted it: if your grant changes mid-pagination, a
replayed cursor is refused with `the caller's scope changed; restart the query` rather than silently re-scoping, so a
changed grant can never replay a stale view.

## Limits and errors

A query defaults to 25 rows and may request up to 100. The query text is size-capped, predicate nesting is depth-capped,
and a query over an unbounded domain, or a join whose key the joined domain cannot serve cheaply, is refused as too
expensive.

| Result | Meaning and fix                                                                                 |
| ------ | ----------------------------------------------------------------------------------------------- |
| `400`  | The query did not parse, is invalid, is over budget, or the cursor no longer matches your scope |
| `401`  | No valid credential was presented                                                               |
| `404`  | You cannot read the domain; its existence is not disclosed                                      |
| `415`  | The request body was not `application/json`                                                     |
| `422`  | The JSON body was malformed or carried an unknown field                                         |
| `503`  | The query backend was unavailable                                                               |

Error bodies never echo your query text or parameter values, so a failed query cannot reflect a secret you embedded in a
predicate back to you.
