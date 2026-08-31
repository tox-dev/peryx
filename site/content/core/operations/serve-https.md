+++
title = "Serve HTTPS"
description = "Use a supplied certificate, ACME, or a trusted reverse proxy to serve HTTPS."
weight = 12
+++

peryx serves HTTP until you configure TLS. Use HTTPS when a client crosses a host or network boundary. Start with a
running server from [Getting started](@/core/start/getting-started.md).

## Supply a certificate

Configure a PEM certificate chain and private key under `[tls]`:

```toml
[tls]
cert = "/etc/peryx/fullchain.pem"
key = "/etc/peryx/privkey.pem"
```

peryx serves HTTP/2 on the configured port. The client must trust the certificate authority. Public certificate
authorities work with standard trust stores; install private authorities in each client environment.

## Use ACME

An `[acme]` table obtains and renews a certificate:

```toml
[acme]
domains = ["packages.example.com"]
contact = "admin@example.com"
cache-dir = "/var/lib/peryx/acme"
staging = false
```

DNS must resolve each domain to the server, and the ACME challenge must reach port 443. Set `staging = true` while
checking the deployment. The staging authority issues untrusted certificates and has a separate rate limit.

`[tls]` and `[acme]` cannot appear together.

## Terminate TLS at a reverse proxy

A load balancer or reverse proxy can hold the certificate and forward HTTP to peryx on a private network. The proxy must
replace caller-supplied forwarding headers:

```nginx
location / {
    client_max_body_size 10g;
    proxy_http_version 1.1;
    proxy_request_buffering off;
    proxy_send_timeout 5m;
    proxy_read_timeout 5m;
    proxy_set_header Host $http_host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_pass http://127.0.0.1:4433;
}
```

`client_max_body_size 10g` caps each proxied request at 10 GiB. nginx returns `413 Request Entity Too Large` before
forwarding a larger body. Set the ceiling above the largest artifact and its multipart overhead; peryx applies
`max_artifact_size_bytes` and repository quotas after it receives the stream. Setting the size to `0` disables nginx's
body-size check. See nginx's
[`client_max_body_size`](https://nginx.org/en/docs/http/ngx_http_core_module.html#client_max_body_size) reference.

`proxy_request_buffering off` sends a request body to peryx as nginx receives it. Keep `proxy_http_version 1.1`
explicit: nginx otherwise buffers a chunked HTTP/1.1 request when the upstream proxy connection uses HTTP/1.0. The
five-minute `proxy_send_timeout` and `proxy_read_timeout` values limit idle gaps between writes to peryx and reads from
peryx. They do not limit the total duration of a transfer that continues to make progress. Increase them if the private
network or post-upload processing can stay idle for more than five minutes. See nginx's
[`proxy_request_buffering`](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_request_buffering),
[`proxy_send_timeout`](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_send_timeout), and
[`proxy_read_timeout`](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_read_timeout) references.

`$http_host` preserves the client's `Host` header, including an explicit port such as `registry.example:8443`. peryx
uses that authority for the OCI token realm and generated client URLs. nginx's
[`proxy_set_header`](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_set_header) reference distinguishes
`$http_host` from `$host`.

Trust the proxy address in `peryx.toml`:

```toml
[rate_limit]
enabled = true
trusted_proxies = ["127.0.0.1/32"]
```

For a proxy chain, each proxy must append its immediate peer after the edge replaces caller input. Add proxy networks to
`trusted_proxies`, but keep client networks out. Prevent direct access to the peryx listener from outside the private
network.

## Browser response headers

Every response carries `X-Content-Type-Options: nosniff`, so a browser reads an artifact as the type peryx declared
rather than one it guesses from the bytes. A rendered page also carries
`Content-Security-Policy: frame-ancestors 'none'; base-uri 'none'; object-src 'none'`, `X-Frame-Options: DENY` and
`Referrer-Policy: no-referrer`, which keep another origin from framing a management page and clicking through it. A
handler that sets one of these itself keeps its own value, and no cache header changes.

peryx adds `Strict-Transport-Security: max-age=31536000` only when the connection is HTTPS: either peryx terminates TLS
under `[tls]`, or a proxy listed in `trusted_proxies` forwarded `X-Forwarded-Proto: https`. An `X-Forwarded-Proto` from
any other peer is ignored, so an untrusted caller cannot pin a host that peryx serves over cleartext. The header claims
only the host the client dialled; add `includeSubDomains` at the proxy when you own every name below it.

## Configure clients

Each [ecosystem guide](@/ecosystems/_index.md) lists its client URLs and certificate-store requirements.

See the [configuration reference](@/core/operations/configuration.md#tls) for TLS and ACME settings.
