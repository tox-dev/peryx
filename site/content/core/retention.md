+++
title = "Retention plans"
description = "Evaluate an index's retention rules into a deterministic, side-effect-free removal plan."
weight = 10
+++

A retention plan names which artifacts an index keeps and which become eligible for removal. Evaluation reads one
metadata snapshot, applies the configured rules, and returns an ordered decision per artifact. It changes no metadata
and touches no blob. This release ships the planner, which computes and reports decisions. Applying a plan, exposing it
over HTTP or the CLI, and reclaiming bytes come later and consume this planner without rebuilding it.

The subject is an index's hosted upload records. Cached upstream pages are evicted through cache maintenance, and
unreferenced blobs are reclaimed through blob collection; neither is a retention decision.

## Rules

A policy holds two ordered rule groups. `keep` rules protect an artifact; `expire` rules mark it for removal. A rule
matches one dimension:

| Rule             | Matches                                                        |
| ---------------- | -------------------------------------------------------------- |
| `age`            | An artifact published at least `older_than_seconds` before now |
| `source`         | An artifact routed from the named source                       |
| `project-prefix` | An artifact whose project name begins with `prefix`            |
| `keep-latest`    | An artifact among the newest `count` versions of its project   |
| `cached`         | A cached artifact                                              |
| `trash`          | A soft-deleted artifact                                        |
| `orphan`         | An artifact no live reference reaches                          |

The same rule protects in `keep` and removes in `expire`; the group gives it meaning. An `age` rule matches nothing when
the artifact carries no publish time or the evaluation supplies no clock, so the planner ages only what it can date.

Rules load from configuration as a tagged list:

```toml
keep = [
  { selector = "keep-latest", count = 10 },
  { selector = "age", older_than_seconds = 2592000 },
]
expire = [
  { selector = "trash" },
  { selector = "project-prefix", prefix = "scratch-" },
]
```

## Precedence

A `keep` rule always wins over an `expire` rule, the precedence
[Google Artifact Registry cleanup policies](https://cloud.google.com/artifact-registry/docs/repositories/cleanup-policy)
define. The planner evaluates each artifact in order: the first matching `keep` rule retains it; otherwise the first
matching `expire` rule removes it; otherwise it is retained with no rule. Each decision names the rule that decided it,
so an operator reads why an artifact survived a policy that could have removed it.

## Version ordering

Versions rank newest first within a project. Python versions order under [PEP 440](https://peps.python.org/pep-0440/),
so `2.0` outranks `2.0rc1` and `2.0+local` outranks `2.0`. Two spellings of one release (`1.0` and `1.0.0`) collapse to
one rank, so `keep-latest` counts releases, not filenames. A version that is not valid PEP 440 ranks after every valid
one, ordered by its string, so a legacy spelling still gets a stable, documented position rather than an arbitrary one.

`keep-latest` reads this rank: `count = 10` protects the ten newest releases and their files.

## Output

Evaluation streams one decision per artifact, ordered newest release first, then by filename, then by digest. The order
is total, so repeating an evaluation over the same snapshot and policy produces byte-identical output.

Each decision records the artifact's project, version, filename, digest, storage class, and logical visibility (active,
yanked, or hidden). A removal decision adds:

- `outcome`: `remove`, against `retain` for a kept artifact.
- `rule`: the rule that decided it.
- `bytes`: the artifact's estimated physical size, the capacity a removal would reclaim.
- `retained_alternatives`: the project's surviving versions, so a reader sees what a removal leaves in place.

## Snapshot and policy identity

A plan carries the identity of both inputs it read, so a later apply step can reject a plan built against stale state:

- `policy_version`: a stable content hash of the compiled rules. Equal rules produce an equal version; any rule change
  produces a different one.
- `frontier`: the metadata generation the scan read, combining the repository serial, the catalog generation, and the
  policy generation. It mirrors the store's policy-input generation.

## Side-effect-free contract

Evaluation opens read transactions only. It reads indexed metadata and digest references and never enumerates backend
blobs. It groups one project at a time and streams that project's decisions before reading the next, so a large index
never holds as one in-memory plan. A dropped connection, a cancelled request, or a crash stops the scan mid-pass, and
the store keeps the state it already held, because the scan wrote none.
