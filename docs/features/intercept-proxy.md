---
layout: default
title: Intercept Proxy (TLS-MITM)
parent: Features
nav_order: 16
---

# Intercept / Redirect Proxy Mode

Mock an external **HTTPS** dependency whose target host the system-under-test (SUT) **hard-codes** —
for example a feature-flag SDK that always fetches its config from
`https://cdn.example.com/config.json`. You can't point such a client at a mock port, so Rift can sit
in the request path as a **forward proxy**, terminate the TLS call, match it with the ordinary
predicate engine, and either serve a stub inline or forward it to one of your imposters.

This replaces the usual **mitmproxy** sidecar (a container, a Python redirect addon, and checked-in
CA/key/truststore files) with a couple of Rift admin calls and **no committed crypto**.

> This is the Rift-native mechanism. The `zio-bdd` `MockControl` `Intercept` capability
> (EtaCassiopeia/zio-bdd#219) wraps it for BDD tests via `EmbeddedRift`.

---

## How it works

1. The SUT is pointed at Rift's **intercept listener** as its HTTPS proxy
   (`https.proxyHost` / `https.proxyPort`).
2. The SUT issues `CONNECT cdn.example.com:443`; Rift answers `200 Connection Established` and
   **TLS-terminates** the tunnel, minting a per-host leaf certificate on the fly, signed by an
   **intercept CA** Rift generates at startup.
3. The decrypted request is matched against **intercept rules** using the same
   [predicate engine]({{ site.baseurl }}/features/) as imposters.
4. A matching rule either **serves an inline stub** or **forwards** the request to one of your
   imposters on `127.0.0.1:<port>`.

The one constraint TLS-MITM cannot remove: **the SUT must trust the intercept CA.** Rift automates
provisioning that trust (it emits a CA cert and a ready-to-use truststore) — see
[Trusting the CA](#trusting-the-ca-from-the-sut).

---

## Enabling the listener

The intercept listener is an opt-in, embedder-facing API — nothing runs until you start it, so the
default imposter-on-a-port model is unchanged. A minimal, complete, runnable example lives in
[`crates/rift-http-proxy/tests/intercept_config_cdn_example.rs`](https://github.com/EtaCassiopeia/rift/blob/master/crates/rift-http-proxy/tests/intercept_config_cdn_example.rs)
(run it with `cargo test -p rift-http-proxy --test intercept_config_cdn_example`). In outline:

```rust
use std::sync::Arc;
use rift_core::proxy::intercept_ca::{CertificateAuthority, SniCertResolver};
use rift_http_proxy::intercept::InterceptListener;
use rift_http_proxy::intercept_rules::InterceptRules;

let ca = Arc::new(CertificateAuthority::generate()?);      // or CertificateAuthority::load_pem(cert, key)
let rules = InterceptRules::new();
let resolver = Arc::new(SniCertResolver::new(ca.clone())); // mints one leaf per SNI host
let listener = InterceptListener::bind("127.0.0.1:0".parse()?, resolver, rules.clone()).await?;
// Point the SUT at `listener.local_addr()` as its HTTPS proxy, trusting `ca`.
```

To expose rule configuration and CA export over the admin API, build the admin server
`with_intercept(Arc::new(InterceptState { rules, ca }))`.

---

## Configuring rules (admin API)

A rule matches an intercepted request by **host** (exact, case-insensitive; omit for any) and
**predicates** (the usual Mountebank predicate JSON, AND-ed), and carries one action.

### Serve an inline stub

```bash
curl -X POST http://localhost:2525/intercept/rules -d '{
  "host": "cdn.example.com",
  "predicates": [{ "equals": { "path": "/config.json" } }],
  "action": { "serve": {
    "statusCode": 200,
    "headers": { "content-type": "application/json" },
    "body": "{\"featureX\":\"ON\"}"
  }}
}'
```

### Forward to one of your imposters

```bash
# The SUT's HTTPS call is decrypted and forwarded to the imposter on 127.0.0.1:4545,
# which does its own predicate/space matching and returns the response.
curl -X POST http://localhost:2525/intercept/rules -d '{
  "host": "cdn.example.com",
  "action": { "forward": { "port": 4545 } }
}'
```

| Verb & path | Effect |
|:--|:--|
| `POST /intercept/rules` | Add one rule (object) or many (array) |
| `GET /intercept/rules` | List all rules |
| `DELETE /intercept/rules` | Remove all rules |

When no rule matches, the request falls through to a default `200` (so an unconfigured host is
answered rather than hanging). Non-goals: HTTP/2, WebSockets, and chunked request bodies are not
decoded (see [Limitations](#limitations)).

---

## Trusting the CA from the SUT

Export the CA and a ready-to-use truststore — nothing crypto needs to live in your repo:

```bash
curl http://localhost:2525/intercept/ca.pem -o rift-ca.pem                       # PEM
curl "http://localhost:2525/intercept/truststore.p12?password=changeit" -o ts.p12 # PKCS#12
curl "http://localhost:2525/intercept/truststore.jks?password=changeit" -o ts.jks # JKS (JVM)
```

The truststore endpoints return the store bytes plus an `x-truststore-password` response header
echoing the password used (default `changeit`, override with `?password=`).

**JVM SUT — one-line wiring** (trust the CA and route HTTPS through the intercept listener):

```
-Djavax.net.ssl.trustStore=ts.jks -Djavax.net.ssl.trustStorePassword=changeit \
-Dhttps.proxyHost=<rift-host> -Dhttps.proxyPort=<intercept-port>
```

---

## What this replaces

The classic mitmproxy setup — a `mitmproxy` container, a Python `request()` redirect addon, and
committed CA cert + private key + dhparams + JKS truststore — collapses to: generate a CA (or load
one), post a rule, and hand the SUT the emitted truststore. Fewer moving parts, **no committed
private keys**, and it works identically for the container and embedded adapters.

---

## Limitations

- **The SUT must trust the intercept CA** — this is inherent to HTTPS MITM; Rift only automates
  provisioning it.
- **Not a general mitmproxy replacement** — no HTTP/2 / h2c, WebSocket proxying, or flow scripting.
- **Request bodies are read only when `Content-Length`-framed** — chunked / streamed request bodies
  are not decoded and are treated as empty for matching and forwarding (logged at `warn`).
- **Forward-proxy (`CONNECT`) only** — transparent interception is not implemented.
- The listener is started by an **embedder** (or the zio-bdd adapter); a CLI flag to start it from
  the standalone `rift` binary is a follow-up.
