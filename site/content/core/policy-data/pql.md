+++
title = "Query language (PQL)"
description = "Run bounded, read-only queries over typed operational data."
weight = 8
+++

PQL, the Peryx Query Language, provides one read surface over operational state. `POST /+query` accepts query text and
parameters, then returns a bounded page of typed rows. The language supports selection, filtering, ordering,
aggregation, joins, and pagination. It has no mutation operations.

PQL selects structured metadata. Ranked text search remains available through `/+search`.

## Syntax

```text
from <domain>
[ where <predicate> ]
[ select <field> [, <field> ...] ]
[ aggregate <func>(<column>) as <alias> [, ...] by <key> [, ...] ]
[ order by <field> [asc|desc] [, ...] ]
[ limit <n> ]
```

Predicates support `and`, `or`, `not`, parentheses, comparisons, membership with `in (...)`, and prefix matching with
`starts_with`. PQL has no arithmetic, function calls, or leading-wildcard matches. The planner checks query cost before
execution.

Literal forms include strings, integers, booleans, and RFC 3339 timestamps prefixed with `@`, such as
`@2026-06-01T00:00:00Z`.

Bind caller values through named parameters. The evaluator fills a `:name` placeholder from `params`; parameter values
cannot change query structure.

```console
curl -u alice:$PASSWORD \
  -H 'content-type: application/json' \
  -d '{
        "query": "from policy.decisions where repository == :repo and state == \"deny\" order by evaluated_at desc limit 25",
        "params": {"repo": "team-cache"}
      }' \
  http://127.0.0.1:4433/+query
```

The response contains rows and an optional cursor:

```json
{
  "rows": [
    {
      "repository": "team-cache",
      "resource": "component-a",
      "state": "deny",
      "action": "serve",
      "evaluated_at": 1800000000,
      "fresh": true
    }
  ],
  "next_cursor": null
}
```

## Domains

PQL provides two typed domains:

- `policy.decisions`: bounded repository policy history
- `usage.reads`: durable read and byte totals by repository resource

### `policy.decisions`

| Column                     | Type      | Meaning                                                                |
| -------------------------- | --------- | ---------------------------------------------------------------------- |
| `repository`, `resource`   | string    | Shared repository and owner-normalized resource                        |
| `group`, `artifact`        | string    | Optional owner-supplied grouping and artifact identity                 |
| `action`                   | string    | Evaluated operation                                                    |
| `state`                    | string    | `allow`, `deny`, or `wait`                                             |
| `evaluated_at`             | timestamp | Evaluation time in whole Unix seconds                                  |
| `fresh`                    | bool      | Current repository, catalog, and policy generations match              |
| `source`, `rule`, `reason` | string    | Routed source, matched rule, and explanation; operator access required |

### `usage.reads`

| Column                   | Type   | Meaning                                         |
| ------------------------ | ------ | ----------------------------------------------- |
| `repository`, `resource` | string | Repository and resource                         |
| `reads`                  | int    | Lifetime artifact reads served                  |
| `bytes`                  | int    | Lifetime bytes served; operator access required |

`select *` and an omitted `select` return all columns visible to the caller. A named selection returns those visible
columns. Default ordering uses descending `evaluated_at` for policy decisions and descending `reads` for usage.

## Aggregation

PQL supports `count`, `sum`, `min`, and `max` over declared numeric columns, grouped by declared keys.

```console
curl -u alice:$PASSWORD \
  -H 'content-type: application/json' \
  -d '{"query": "from policy.decisions aggregate count() as decisions by state"}' \
  http://127.0.0.1:4433/+query
```

Use `/+analytics/timeline` for time buckets.

## Joins

A query can join two domains on declared shared keys. Joins use inner semantics: an outer row appears after a match in
the joined domain.

```console
curl -u alice:$PASSWORD \
  -H 'content-type: application/json' \
  -d '{"query": "from policy.decisions join usage.reads on repository, resource where state == \"deny\" order by reads desc limit 25"}' \
  http://127.0.0.1:4433/+query
```

The planner admits a join when the joined domain indexes all join keys. It rejects a join that requires an unbounded
scan with `400`. A field present on either side keeps the stricter visibility class, and a join key above the caller's
class is refused, since an inner join discloses that key's value through the rows it keeps.

Keys must also be able to narrow the joined domain. A domain joined to itself is refused, as is a join keyed only on
`repository` once the query is pinned to a repository, whether by the caller's grant or by a `repository ==` filter:
both pair every row with every row sharing its key, so the output is the product of the two domains rather than a
lookup. A join that reaches 25,000 key matches is refused with `400` for the same reason, before those rows are held in
memory.

A join stops as soon as it holds the requested page, so its cost follows the page rather than the whole match set. That
holds when the page is all the query reads: no aggregate, and every order term on a column the outer domain carries. The
example above orders on `reads`, a `usage.reads` column, so it reads every match up to the cap.

## Authorization

The evaluator resolves the caller credential and adds a repository predicate before ordering and pagination. Query text
cannot name or remove that predicate.

Filter `repository` by its configured repository name, which is the value stored in grants and records. A repository
credential or reader can query its granted repository. An administrator can omit that filter for operator-wide data. A
caller without domain access receives `404`.

An index's `anonymous_read` setting governs artifact serving alone. A repository credential reaches this endpoint only
when its password resolves to a live token holding a `read` grant over the whole repository. A credential that resolves
to no such token receives `401`, whether or not the repository serves artifacts anonymously. The same rule covers
`/+analytics`, `/+analytics/completeness`, and `/+quota`.

Repository-scoped callers see repository fields. A column above the caller's class is unknown to them: naming it in a
selection, a predicate, an order term, a group key, or an aggregate is a `400`, with the same error a misspelled column
gives. Filtering on `usage.reads.bytes` or on `policy.decisions.source`, `rule`, and `reason` therefore cannot disclose
those values through which rows come back, and a rejected query reads no rows. Operator fields are also omitted from the
response, so serialization stays a second boundary. Responses containing operator fields use `Cache-Control: no-store`;
repository-level responses use `private, no-cache`.

## Pagination

A page with more matches includes `next_cursor`. Send it as the request `cursor` to continue. The cursor binds to the
authorization scope that created it. A grant change causes `the caller's scope changed; restart the query`.

## Limits and errors

The default page size is 25, with a maximum of 100. Query text size, predicate depth, and execution cost have fixed
bounds.

| Result | Meaning                                          |
| ------ | ------------------------------------------------ |
| `400`  | Parse, validation, budget, join, or cursor error |
| `401`  | Missing or invalid credential                    |
| `404`  | Caller cannot read the domain                    |
| `415`  | Request body does not use `application/json`     |
| `422`  | Malformed JSON or unknown body field             |
| `503`  | Query backend unavailable                        |

Error bodies omit query text and parameter values.
