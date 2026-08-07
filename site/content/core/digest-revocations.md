+++
title = "Digest revocations"
description = "Block immutable content by digest while retaining incident evidence."
weight = 9
+++

A digest revocation records a server-wide decision about immutable content. The active record keeps its reason,
revision, timestamps, and the stable user ID of the administrator who changed it.

Revocation does not delete bytes or alter retention state. Discovery and download behavior belongs to each ecosystem:

- [Revoked Python package content](@/ecosystems/pypi/revoked-content.md)
- [Revoked OCI content](@/ecosystems/oci/revoked-content.md)

## Command-line management

The CLI calls the live management API. It reads the administrator password from standard input or a secret file. It does
not accept the password as an argument or environment value. Input and responses have a 1 MiB limit. The client removes
one terminal LF or CRLF, rejects redirects, and permits HTTP for loopback addresses.

```console
$ peryx revocation put \
    --server https://packages.example \
    --user admin \
    --password-file /run/secrets/peryx-admin-password \
    --reason 'compromised build host' \
    <digest>
$ peryx revocation inspect \
    --server https://packages.example \
    --user admin \
    --password-file /run/secrets/peryx-admin-password \
    <digest>
$ peryx revocation list \
    --server https://packages.example \
    --user admin \
    --password-file /run/secrets/peryx-admin-password \
    --status active \
    --limit 25
$ peryx revocation lift \
    --server https://packages.example \
    --user admin \
    --password-file /run/secrets/peryx-admin-password \
    <digest>
```

Commands print JSON. `put` creates an absent record. Repeating `put` with the active reason returns the same record; a
different reason conflicts. Putting a lifted digest opens a new revision. Repeating `lift` returns the existing lifted
record.

## API and authorization

The API uses local-user HTTP Basic authentication. Inspect and list require `administration:read`; put and lift require
`administration:write`. The server takes the actor ID from the authenticated user and rejects actor fields supplied by
the caller.

| Operation                | Request                                               |
| ------------------------ | ----------------------------------------------------- |
| Create, retry, or reopen | `PUT /+revocations/{digest}` with `{"reason":"..."}`  |
| Inspect                  | `GET /+revocations/{digest}`                          |
| List                     | `GET /+revocations?status=active&limit=25&cursor=...` |
| Lift                     | `POST /+revocations/{digest}/lift`                    |

Reasons must contain non-whitespace text and cannot exceed 2,048 UTF-8 bytes. Management responses use
`Cache-Control: no-store`. Security events include the actor, digest, and transition result, but omit the free-form
reason and credentials.

Authentication and authorization run before digest parsing or lookup. Invalid credentials receive one generic challenge.
An authenticated user without administrator authority receives `404 Not Found` for valid and malformed targets.

## Decision cache

The serving service caches clear and revoked decisions for 60 seconds with a fixed capacity. Mutation commits invalidate
the changed digest before releasing the write gate, which prevents an older clear decision from replacing the new state.

The cache does not retain metadata errors. A failed metadata read returns unavailable, so an ecosystem driver can deny
the content. Cross-process invalidation is outside this lifecycle. Ecosystem responses apply the same 60-second bound to
compliant client and proxy caches.
