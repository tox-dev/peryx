+++
title = "Serve HTTPS"
description = "Turn on TLS with a certificate you provide, an automatic Let's Encrypt certificate, or a reverse proxy that terminates TLS."
weight = 12
+++

peryx serves plain HTTP by default. That is the right choice on a laptop: `pip` and `uv` accept any URL, and `docker`
and `podman` trust a [loopback](@/core/glossary.md#loopback-http) registry over HTTP with no configuration. Serving over
the network is different: browsers and container clients demand HTTPS, and a container client refuses a plain-HTTP
registry that is not loopback unless you weaken its security settings. This guide turns on TLS. It assumes a built
peryx; see [Getting started](@/core/getting-started.md).

TLS is off until you configure it, and an unconfigured server keeps the exact plain-HTTP path, so turning it on costs
nothing until you do. There are three approaches; pick one.

## Bring your own certificate

If you already have a certificate and key (from your organization's CA, `mkcert`, or a previous Let's Encrypt run), a
`[tls]` table serves HTTPS from them:

```toml
# peryx.toml
[tls]
cert = "/etc/peryx/fullchain.pem" # PEM certificate chain
key = "/etc/peryx/privkey.pem"    # PEM private key
```

peryx negotiates HTTP/2 and answers on the same port. A client trusts the connection when the certificate's CA is in its
trust store: a public CA is trusted everywhere; a private or `mkcert` CA must be installed into the client's trust store
(`mkcert -install` does this for the local machine, and Docker Desktop then trusts it too).

## Automatic certificates with ACME

For a public deployment, an `[acme]` table obtains and renews a certificate from Let's Encrypt, so a client trusts it
with no insecure flag and no manual certificate handling:

```toml
[acme]
domains = ["registry.example.com"] # the names to certify; reachable on port 443
contact = "admin@example.com"      # where expiry notices go
cache-dir = "/var/lib/peryx/acme"  # persist issued certs across restarts (default "acme-cache")
staging = false                    # true uses Let's Encrypt staging while testing
```

For this to work the domain's DNS must point at the server and port 443 must be reachable from the internet, since the
ACME challenge happens there. While testing, set `staging = true` to use Let's Encrypt's staging environment. It has
higher rate limits and issues untrusted certificates, so a test run does not spend the production quota. The `[tls]` and
`[acme]` tables are mutually exclusive.

## Terminate TLS at a reverse proxy

If a load balancer, ingress controller, or reverse proxy (nginx, Caddy, a cloud LB) already holds your certificate,
leave both tables unset and let it terminate TLS, forwarding plain HTTP to peryx on a private network. A clustered
deployment usually takes this shape, and it needs no peryx TLS config.

## Point clients at HTTPS

Once peryx serves HTTPS with a trusted certificate, drop the `http://` for `https://` and remove any insecure flag:

```shell
pip install --index-url https://packages.example/root/pypi/simple/ requests
docker pull packages.example/dockerhub/library/alpine:latest
```

## Related

- Every TLS and ACME key: [configuration reference](@/core/configuration.md#tls)
- Run peryx as a container registry: [run a container registry](@/ecosystems/oci/guides/container-registry.md)
