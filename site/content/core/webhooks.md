+++
title = "Webhook API"
description = "Configure signed, asynchronous deliveries for repository state changes."
weight = 11
+++

Webhooks deliver committed repository changes to an HTTP endpoint. Delivery runs outside the mutation request, so an
unreachable target does not hold a publisher connection open.

## Target schema

Each target belongs to one index and contains:

| Field        | Contract                                                                  |
| ------------ | ------------------------------------------------------------------------- |
| `name`       | Unique target name within the index                                       |
| `url`        | HTTP or HTTPS endpoint without user info, query, or fragment              |
| `secret_env` | Environment variable that contains the HMAC secret                        |
| `secret`     | Literal HMAC secret, used instead of `secret_env`                         |
| `events`     | Optional allowlist of event names exposed by the ecosystem implementation |

An empty or omitted event list receives each event the implementation exposes. A target on a virtual route receives
changes admitted through that route, including writes stored by its hosted layer.

## Delivery envelope

Every delivery uses `POST` with a JSON body and these headers:

| Header              | Contract                                    |
| ------------------- | ------------------------------------------- |
| `Content-Type`      | `application/json`                          |
| `User-Agent`        | `peryx/<version>`                           |
| `X-Peryx-Event`     | Event name, equal to the body `event` field |
| `X-Peryx-Delivery`  | Stable delivery identifier across retries   |
| `X-Peryx-Timestamp` | Unix second used to sign this attempt       |
| `X-Peryx-Signature` | `sha256=<hex>` HMAC-SHA256 signature        |

The neutral body fields are:

| Field          | Presence | Meaning                                                |
| -------------- | -------- | ------------------------------------------------------ |
| `event`        | Required | Registered event name                                  |
| `created_at`   | Required | Unix second when the mutation committed                |
| `index`        | Required | Index that accepted the request                        |
| `route`        | Required | Client-facing route                                    |
| `hosted_index` | Required | Hosted layer that stored the change                    |
| `project`      | Required | Ecosystem entity name                                  |
| `version`      | Optional | Ecosystem release, tag, or other reference             |
| `file`         | Optional | Object with implementation-defined filename and sha256 |
| `count`        | Required | Records changed by the mutation                        |
| `actor`        | Optional | Authenticated identity                                 |
| `request_id`   | Optional | Caller request identifier                              |

The implementation defines event names and the meanings of `project`, `version`, and `file`. Consumers must ignore
unknown fields so an implementation can add data without breaking existing receivers.

## Signature contract

peryx signs the exact bytes below with the target secret:

```text
<timestamp>.<delivery-id>.<raw-json-body>
```

Receivers compare the HMAC in constant time and reject timestamps outside their replay window. Re-serializing the body
before verification changes the signed bytes.

## Delivery contract

peryx stores a delivery before a background worker sends it. A process restart retains queued work. Any `2xx` response
marks the delivery complete. A timeout or other status schedules a bounded retry with the same delivery identifier.

Delivery is at least once and does not preserve mutation order. Receivers deduplicate by `X-Peryx-Delivery` and use
`created_at` when event order affects their state.

The delivery log stores the target name, attempt count, next retry, response status, and bounded error text. It excludes
secrets, signatures, credentials, URL queries, and response bodies.

## Ecosystem events

- [PyPI webhook events and payloads](@/ecosystems/pypi/reference/endpoints.md#webhooks)
- [OCI webhook events and payloads](@/ecosystems/oci/reference/endpoints.md#webhooks)
- [Configuration reference](@/core/configuration.md)
