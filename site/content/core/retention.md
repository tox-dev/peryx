+++
title = "Retention plans"
description = "Evaluate repository retention rules without changing metadata or content."
weight = 10
+++

A retention plan evaluates one metadata snapshot and returns an ordered decision for each artifact. Evaluation changes
no metadata and deletes no content. Administrators can inspect the result through the CLI or management API before a
separate apply step uses it.

The planner reads hosted records. Cache eviction manages upstream cache state, and blob reclamation removes unreferenced
bytes. Neither operation changes retention decisions.

## Rules

A policy has ordered `keep` and `expire` groups. A matching `keep` rule protects an artifact. If none matches, the first
matching `expire` rule makes it eligible for removal.

Available selectors include:

- `age`: publish time is at least `older_than_seconds` before the evaluation clock
- `source`: content came from the named source
- `project-prefix`: the driver subject starts with `prefix`
- `keep-latest`: content belongs to one of the newest `count` versions of its subject
- `cached`: content came from an upstream cache
- `trash`: content has a restorable deletion record
- `orphan`: no live metadata reference reaches the content
- `visibility`: content has the named driver visibility state

An `age` rule does not match an artifact without a publish time or an evaluation without a clock.

```toml
keep = [
  { selector = "keep-latest", count = 10 },
  { selector = "age", older_than_seconds = 2592000 },
]
expire = [
  { selector = "trash" },
  { selector = "project-prefix", prefix = "scratch-" },
  { selector = "visibility", state = "hidden" },
]
```

## Precedence

The first matching `keep` rule wins over all `expire` rules. Without a keep match, the first expire match wins. If no
rule matches, the planner retains the artifact and reports no deciding rule.

This precedence follows
[Google Artifact Registry cleanup policies](https://cloud.google.com/artifact-registry/docs/repositories/cleanup-policy).

## Driver ordering

The ecosystem driver ranks versions for `keep-latest` and supplies a stable identity for equivalent versions. The
planner orders versions by that rank, then uses artifact name and digest as tie-breaks.

- [Python package retention ordering](@/ecosystems/pypi/reference/policy.md#retention-ordering)

## Decisions

Each decision contains `project`, `version`, `filename`, `digest`, storage class, and logical visibility. These field
names form the shared planner schema; drivers map their own subjects and content types onto them.

A removal decision also contains:

- `outcome`: `remove`, compared with `retain`
- `rule`: selector that produced the decision
- `bytes`: estimated physical size
- `retained_alternatives`: subject versions left after the planned removal

The output order is total. Evaluating the same snapshot and policy produces the same bytes.

## Plan identity

A plan identifies both inputs:

- `policy_version`: stable hash of the compiled, typed rule values
- `frontier`: repository serial, catalog generation, and policy generation read by the scan

An apply step can reject a plan after either input changes.

## Read-only execution

Evaluation opens read transactions and does not enumerate backend blobs. It groups and emits one subject at a time. A
cancelled request or process exit leaves repository state unchanged.

## HTTP preview

`POST /+retention/plan` returns one page. `POST /+retention/export` streams the complete plan as
[JSON Lines](https://jsonlines.org/) with the summary first. Both require a local administrator. An unauthorized caller
receives `404` and cannot use errors to infer repository contents.

Example request:

```json
{
  "repository": "team-hosted",
  "keep": [
    {
      "selector": "keep-latest",
      "count": 3
    }
  ],
  "expire": [
    {
      "selector": "age",
      "older_than_seconds": 7776000
    }
  ],
  "limit": 100
}
```

Example page:

```json
{
  "summary": {
    "policy_version": 42,
    "frontier": {
      "repository": 7,
      "catalog": 3,
      "policy": 2
    }
  },
  "candidates": [
    {
      "project": "example",
      "version": "1.0",
      "artifact": "example-1.0.bin",
      "digest": "<digest>",
      "class": "hosted",
      "visibility": "active",
      "bytes": 20480,
      "outcome": "remove",
      "rule": "age",
      "retained_alternatives": [
        "2.0"
      ]
    }
  ],
  "next_cursor": null
}
```

The export response uses the plan identity as its `ETag`. A later apply can send that value with `If-Match`.

## CLI preview

`peryx retention dry-run` prints one page of tab-separated candidates, followed by a summary and optional cursor.
`peryx retention export` writes the JSON Lines form. Both read the local store and load rules from a TOML file. Without
a rules file, the plan retains all content.

```console
$ peryx retention dry-run --index team-hosted --rules retention.toml --limit 100
$ peryx retention export --index team-hosted --rules retention.toml > plan.jsonl
```

## Pagination and limits

A cursor binds its offset to the plan identity. A changed snapshot causes `409 Conflict` over HTTP or a stale-cursor
error in the CLI. Export resumes from a page boundary by using that page cursor. The response advertises
`Accept-Ranges: none` because byte ranges cannot identify plan boundaries.

A page holds at most its requested limit. Export buffers one candidate and applies backpressure when the reader stalls.
Each repository permits a fixed number of concurrent plans; excess requests receive `429 Too Many Requests`.
