+++
title = "Webhook API"
description = "Configure signed, asynchronous deliveries for repository state changes."
weight = 11
+++

Webhooks deliver committed repository changes to an HTTP endpoint outside the mutation request.

## Target schema

Each target belongs to one index:

| Field        | Contract                                                         |
| ------------ | ---------------------------------------------------------------- |
| `name`       | Unique target name within the index                              |
| `url`        | HTTP or HTTPS endpoint without user info, query, or fragment     |
| `secret_env` | Environment variable that contains the HMAC secret               |
| `secret`     | Literal HMAC secret, used instead of `secret_env`                |
| `events`     | Optional allowlist of event names exposed by the ecosystem owner |

An empty or omitted event list receives each event the owner exposes. Ecosystem guides define event names and payload
schemas.

### Secret strength

Peryx uses the UTF-8 bytes of the resolved `secret` or `secret_env` value as the HMAC-SHA256 key. The value must contain
at least 32 bytes. [RFC 2104 section 3](https://www.rfc-editor.org/rfc/rfc2104.html#section-3) recommends a key at least
as long as the hash output, which is 32 bytes for SHA-256. Length does not supply entropy; generate each target secret
from a cryptographic random source.

Generate 32 random bytes as hexadecimal and create the output with owner-only permissions:

```console
$ umask 077
$ openssl rand -hex 32 > peryx-webhook-secret
```

Set `secret_env` to an environment variable containing that file's value, or set `secret` to the value itself. Peryx
checks the resolved value during startup and `check-config`.

## Delivery envelope

Every delivery uses `POST` with an owner-defined JSON body and these headers:

| Header              | Contract                                  |
| ------------------- | ----------------------------------------- |
| `Content-Type`      | `application/json`                        |
| `User-Agent`        | `peryx/<version>`                         |
| `X-Peryx-Event`     | Owner-defined event name                  |
| `X-Peryx-Delivery`  | Stable delivery identifier across retries |
| `X-Peryx-Timestamp` | Unix second used to sign this attempt     |
| `X-Peryx-Signature` | `sha256=<hex>` HMAC-SHA256 signature      |

Consumers must ignore unknown payload fields so implementations can extend their schemas without breaking receivers.

## Signature contract

peryx signs these exact bytes with the target secret:

```text
<timestamp>.<delivery-id>.<raw-json-body>
```

Receivers compare the HMAC in constant time and reject timestamps outside their replay window. Re-serializing the body
before verification changes the signed bytes.

## Delivery contract

peryx stores each delivery before a background worker sends it. A process restart retains queued work. A `2xx` response
completes delivery. Transport failures and HTTP `5xx` responses retry with the same delivery ID; `408` and `429` use the
same retry path. A valid `Retry-After` response delays the next attempt when it is later than peryx's local backoff, and
the stored deadline survives a process restart. Other `4xx` responses are final.

Redirects are final after the first attempt. peryx neither follows nor retries them because sending the signed payload
to a target-selected location could move it outside the configured origin. A `302` reports
`webhook target returned redirect 302; redirects are not followed` as its outcome.

Delivery is at least once and does not preserve mutation order. If the receiver accepts a request but peryx loses the
process before recording its result, the next process may send the request again. Retries keep the same
`X-Peryx-Delivery` value so receivers can deduplicate them.

## Retention

The metadata database holds a row only while a delivery is outstanding: queued, in flight, or waiting on a retry
deadline. A delivery that succeeds or exhausts its attempts leaves no row, so the database tracks pending work rather
than lifetime event volume.

Each attempt's outcome goes to the `peryx::webhook` tracing target instead: delivery identifier, index, target name,
event name, attempt count, final status, response status, next retry, and bounded error text. It excludes payloads,
secrets, signatures, credentials, URL queries, and response bodies. Collect that target to keep delivery history for as
long as your operations require.

## Related

- [Ecosystem guides](@/ecosystems/_index.md)
- [Configuration reference](@/core/operations/configuration.md)
