+++
title = "Shadowed candidates"
description = "Inspect the Python distribution selected from a virtual index and the candidates it shadows."
weight = 45
+++

A virtual PyPI repository selects one member for each distribution filename. Other members that offer the same filename,
or that fallback policy excludes, are shadowed. The inspection endpoint replays this resolution from stored state and
reports the selected and shadowed candidates.

The query covers one virtual repository and one normalized project. It reads hosted uploads and cached project pages. It
does not fetch an upstream page, and a cached member without a stored page contributes no candidates.

## Candidate fields

| Field      | Meaning                                                               |
| ---------- | --------------------------------------------------------------------- |
| `member`   | Configured member repository                                          |
| `source`   | `hosted` for an upload or `cached` for upstream content               |
| `filename` | Python distribution filename                                          |
| `digest`   | Distribution content digest, when present                             |
| `selected` | Whether the virtual repository serves this candidate                  |
| `reason`   | Shadow reason, absent on the selected candidate                       |
| `decision` | Last recorded filename-policy result, absent before policy evaluation |

`precedence` means an earlier member supplied the filename. Hosted members precede cached members, so an upload can
shadow an upstream file with the same name. `fallback` means the configured fallback mode excluded the member:

- `private-first` excludes cached candidates when a hosted member contains the project.
- `no-fallback` excludes cached members.
- `fallback` merges distinct filenames and uses precedence for collisions.

`protected-name` means the repository's `protected_names` rules cover the project, which denies upstream fallback for it
in every mode. This rule outranks the fallback mode, so a protected project reports `protected-name` rather than
`fallback` even under `private-first`. Resolution excludes the same candidates, so a protected project with no hosted
member answers a policy denial and no candidate is selected.

The optional `decision` object describes whether policy permits the candidate:

| Field                   | Meaning                                                       |
| ----------------------- | ------------------------------------------------------------- |
| `state`                 | `allow`, `deny`, or `wait`                                    |
| `rule`                  | Matched policy rule                                           |
| `reason`                | Stored explanation with upstream URLs and credentials removed |
| `evaluated_at_unix`     | Evaluation time in Unix seconds                               |
| `next_eligible_at_unix` | Earliest retry time for `wait`                                |
| `fresh`                 | Whether the decision uses the current policy generation       |

A denied candidate remains blocked if it wins filename resolution. A waiting candidate remains held until its retry
time. A stale decision reflects an earlier policy generation and changes after the next evaluation.

## Request and response

Use a repository token or local login:

```console
curl -u __token__:$TOKEN \
  'http://127.0.0.1:4433/+shadow/candidates?repository=root/pypi&project=example'
```

The selected candidate leads its filename group:

```json
{
  "candidates": [
    {
      "member": "hosted",
      "source": "hosted",
      "filename": "example-1.0-py3-none-any.whl",
      "digest": "sha256:1111\u2026",
      "selected": true
    },
    {
      "member": "pypi",
      "source": "cached",
      "filename": "example-1.0-py3-none-any.whl",
      "digest": "sha256:2222\u2026",
      "selected": false,
      "reason": "precedence",
      "decision": {
        "state": "deny",
        "rule": "blocked-project",
        "reason": "project is blocked by policy",
        "evaluated_at_unix": 1700000000,
        "fresh": true
      }
    }
  ],
  "next_cursor": null
}
```

The endpoint requires `repository` and `project`. Peryx normalizes the project under
[PEP 503](https://packaging.python.org/en/latest/specifications/name-normalization/). Results sort by filename, selected
candidate, then member name.

`limit` defaults to 25 and accepts values from 1 through 100. Send `next_cursor` as `cursor` for the next page. The
cursor contains the stable candidate identity, which prevents skipped or repeated candidates across a page boundary.

## Authorization and caching

Peryx authorizes the caller before reading candidates. Local repository readers, publishers, administrators, and a
repository upload token under `__token__` can inspect granted repositories. The server operator role has no implicit
repository access.

Anonymous requests receive `401 Unauthorized`. A missing repository and one outside an authenticated user's grant both
return `404 Not Found`. Responses omit upstream URLs, credentials, authorization headers, and client addresses. They use
`Cache-Control: no-store`.

The endpoint does not change Simple HTML or JSON responses. Installers continue to receive the selected candidate for
each filename.

## Admin UI

`/admin/shadow` displays the same query. Enter a local login or `__token__` repository token, repository route, and
normalized project. The table reports selection, source, member, filename, digest, shadow reason, and policy result.
Previous and Next controls use the API cursor.

Each state has a text label. Selection reads `Selected` or `Shadowed`; sources read `hosted upload` or
`cached upstream`; decisions read `Allowed`, `Denied`, `Waiting`, or a dash. A decision from an earlier policy
generation includes `Stale`. Colour reinforces these labels and does not replace them, satisfying the
[WCAG use-of-color requirement](https://www.w3.org/WAI/WCAG22/Understanding/use-of-color.html).

Credentials remain in the browser tab. The page sends them through the authorization header and does not place them in
the URL or browser storage. Policy text renders as text rather than markup.

## Troubleshooting

Use HTTPS for passwords and repository tokens unless the server listens on loopback.

| Result                      | Check                                                                                         |
| --------------------------- | --------------------------------------------------------------------------------------------- |
| No candidates               | Confirm the repository is virtual and the project resolves; cached members need a stored page |
| `400 Bad Request`           | Use a limit from 1 through 100, a cursor from this query, and a project within 512 bytes      |
| `401 Unauthorized`          | Provide a local login or use `__token__` with a repository token                              |
| `403 Forbidden`             | Give the repository token a write grant                                                       |
| `404 Not Found`             | Check the repository route and local-user grant                                               |
| `500 Internal Server Error` | Inspect the metadata store and server log for member resolution failure                       |
| `503 Service Unavailable`   | Restore user, grant, or authentication storage                                                |
