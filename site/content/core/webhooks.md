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
completes delivery. Transport failures, redirects, `408`, `429`, and `5xx` responses retry with the same delivery ID.
Other `4xx` responses are final. peryx does not follow redirects.

Delivery is at least once and does not preserve mutation order. Receivers deduplicate by `X-Peryx-Delivery`.

The delivery log stores the target name, attempt count, next retry, response status, and bounded error text. It excludes
secrets, signatures, credentials, URL queries, and response bodies.

## Related

- [Ecosystem guides](@/ecosystems/_index.md)
- [Configuration reference](@/core/configuration.md)
