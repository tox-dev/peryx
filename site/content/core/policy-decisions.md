+++
title = "Policy decisions"
description = "Inspect bounded policy decision history without exposing credentials or cross-repository data."
weight = 8
+++

Peryx records the result each time the runtime evaluates an index policy. The record supports incident review and policy
debugging after the request ends. It does not replace request-time evaluation: serving code evaluates the current policy
and writes the result, while stored decisions remain an audit resource.

Each record contains the repository, resource, optional group and artifact, routed source, action, result, matched rule,
reason, evaluation time, input generations, and next eligible time. `allow`, `deny`, and `wait` use one schema. A UUID
identifies the decision; pagination uses a separate cursor that is not part of the decision identity.

| Field                    | Meaning                                                                 |
| ------------------------ | ----------------------------------------------------------------------- |
| `repository`             | Stable configured repository name; queries select its route             |
| `resource`               | Owner-normalized resource evaluated by the policy                       |
| `group`, `artifact`      | Optional owner-supplied grouping and artifact identity                  |
| `source`                 | Configured route source name, without an upstream URL or credentials    |
| `action`                 | Operation evaluated                                                     |
| `state`                  | `allow`, `deny`, or `wait`                                              |
| `rule`, `reason`         | Matched rule identifier and its bounded explanation                     |
| `evaluated_at_unix`      | UTC Unix timestamp of the evaluation                                    |
| `next_eligible_at_unix`  | Earliest retry time for a waiting decision, when known                  |
| `fresh`                  | Whether current repository, catalog, and policy generations still match |
| `id`, `input_generation` | Audit identity and the generation counters used by freshness checks     |

The input generation has three counters. `repository` follows the durable metadata serial, `catalog` changes when a new
remote catalog becomes active, and `policy` changes when the process loads an index policy. `fresh: false` means at
least one current counter differs from the counters used for that decision. Clients must not use a stale record to
predict a new request.

Query one repository with its access token:

```console
curl -u __token__:$TOKEN \
  'http://127.0.0.1:4433/+policy/decisions?repository=private&state=deny&limit=25'
```

The endpoint accepts `state`, `rule`, `source`, `from`, and `to` filters. Results use newest-first order. Pass
`next_cursor` as `cursor` for the next page. `limit` defaults to 25 and accepts 1 through 100. A cursor belongs to the
same repository and filter set that produced it; changing filters while reusing a cursor can skip matching records.

Peryx retains 10,000 decision records per metadata store. New records remove the oldest history and any current pointer
to a removed record. Reasons stop at 2,048 bytes; values for repository, resource, group, artifact, source, or rule stop
at 512 bytes. Repository, rule, and source query filters use the same 512-byte bound. These limits bound query work and
stored audit data.

Authorization runs before the history query. An access token can inspect only its repository. Local administrators can
omit `repository` to query all repositories or select one by its route. A repository reader or publisher must select a
repository covered by that user's grant. Server operators do not carry repository access. Peryx returns the same
`404 Not Found` for a missing repository and one outside an authenticated user's reach.

Use the reserved `__token__` username for repository-token access. Peryx does not treat that username as a local user on
this endpoint, so creating a local user with the same display name cannot disable repository-token inspection.

The read-only browser at `/admin/policy-decisions` exposes the same filters and cursor pagination. The page keeps the
username and password in reactive memory and disables password autocomplete. It does not write either value to the URL
or browser storage or include them in server-rendered HTML or error messages. The table labels outcomes as Allowed,
Denied, Waiting, or Stale followed by the recorded outcome, so color is not required to interpret a row.

Records exclude credentials, authorization headers, client addresses, and raw policy input. Rule reasons should describe
matched owner facts; they must not include secrets from configuration or requests.

## Troubleshooting

Send local passwords and repository tokens over HTTPS, except for a loopback-only server. Configure Peryx TLS or
terminate TLS at a trusted reverse proxy before exposing the decision view. Reloading the page clears the credentials
held by the hydrated form.

| Result                      | Check                                                                                              |
| --------------------------- | -------------------------------------------------------------------------------------------------- |
| No rows                     | Remove filters, then confirm that the server has evaluated policy since the retained history began |
| `400 Bad Request`           | Use a page size from 1 through 100, a cursor from the same filter set, and filters up to 512 bytes |
| `401 Unauthorized`          | Use a local login, or use `__token__` with a repository token and select its repository            |
| `403 Forbidden`             | Give the selected repository token a write grant; read-only tokens cannot inspect policy records   |
| `404 Not Found`             | Check the repository route and local user's grant; Peryx gives both failures the same response     |
| `500 Internal Server Error` | Inspect the metadata store and server log for a policy-decision query failure                      |
| `503 Service Unavailable`   | Restore user, grant, or authentication storage before retrying                                     |

A stale row remains audit history. Trigger the operation again after changing repository data, catalog state, or policy
if you need a decision made from current inputs.
