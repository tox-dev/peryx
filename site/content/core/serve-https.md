+++
title = "Serve HTTPS"
description = "Use a supplied certificate, ACME, or a trusted reverse proxy to serve HTTPS."
weight = 12
+++

peryx serves HTTP until you configure TLS. Use HTTPS when a client crosses a host or network boundary. Start with a
running server from [Getting started](@/core/getting-started.md).

## Supply a certificate

Configure a PEM certificate chain and private key under `[tls]`:

```toml
[tls]
cert = "/etc/peryx/fullchain.pem"
key = "/etc/peryx/privkey.pem"
```

peryx serves HTTP/2 on the configured port. The client must trust the certificate authority. Public certificate
authorities work with standard trust stores; private authorities must be installed in each client environment.

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
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_pass http://127.0.0.1:4433;
}
```

Trust the proxy address in `peryx.toml`:

```toml
[rate_limit]
enabled = true
trusted_proxies = ["127.0.0.1/32"]
```

For a proxy chain, each proxy must append its immediate peer after the edge replaces caller input. Add proxy networks to
`trusted_proxies`, but keep client networks out. Prevent direct access to the peryx listener from outside the private
network.

## Configure clients

Use the ecosystem guide for client URLs, certificate stores, and local-development exceptions:

- [Python package setup](@/ecosystems/pypi/tutorials/getting-started.md)
- [OCI local transport](@/ecosystems/oci/guides/local-transport.md)

See the [configuration reference](@/core/configuration.md#tls) for each TLS and ACME setting.
