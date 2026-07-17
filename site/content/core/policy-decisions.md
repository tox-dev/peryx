+++
title = "Policy decisions"
description = "Inspect bounded policy decision history without exposing credentials or cross-repository data."
weight = 8
+++

Peryx records the result each time the runtime evaluates an index policy. The record supports incident review and policy
debugging after the request ends. It does not replace request-time evaluation: serving code evaluates the current policy
and writes the result, while stored decisions remain an audit resource.

Each record contains the repository, project, version, filename, routed source, action, result, matched rule, reason,
evaluation time, input generations, and next eligible time. `allow`, `deny`, and `wait` use one schema. A UUID
identifies the decision; pagination uses a separate cursor that is not part of the decision identity.

The input generation has three counters. `repository` follows the durable metadata serial, `catalog` changes when a new
remote catalog becomes active, and `policy` changes when the process loads an index policy. `fresh: false` means at
least one current counter differs from the counters used for that decision. Clients must not use a stale record to
predict a new request.

Query one repository with a token that administers it:

```console
curl -u __token__:$TOKEN \
  'http://127.0.0.1:4433/+policy/decisions?repository=private&state=deny&limit=25'
```

The endpoint accepts `state`, `rule`, `source`, `from`, and `to` filters. Results use newest-first order. Pass
`next_cursor` as `cursor` for the next page. `limit` defaults to 25 and accepts 1 through 100. A cursor belongs to the
same repository and filter set that produced it; changing filters while reusing a cursor can skip matching records.

Peryx retains 10,000 decision records per metadata store. New records remove the oldest history and any current pointer
to a removed record. Reasons stop at 2,048 bytes; values for repository, project, version, filename, source, or rule
stop at 512 bytes. These limits bound query work and stored audit data.

Authorization runs before the history query. A repository operator can inspect only a repository covered by its
administrative token. Records exclude credentials, authorization headers, client addresses, and raw policy input. Rule
reasons should describe matched package facts; they must not include secrets from configuration or requests.
