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
use rift_mock_core::proxy::intercept_ca::{CertificateAuthority, SniCertResolver};
use rift_http_proxy::intercept::InterceptListener;
use rift_http_proxy::intercept_rules::InterceptRules;

let ca = Arc::new(CertificateAuthority::generate()?);      // or CertificateAuthority::load_pem(cert, key)
let rules = InterceptRules::new();
let resolver = Arc::new(SniCertResolver::new(ca.clone())); // mints one leaf per SNI host
let listener = InterceptListener::bind("127.0.0.1:0".parse()?, resolver, rules.clone()).await?;
// Point the SUT at `listener.local_addr()` as its HTTPS proxy, trusting `ca`.
```

To expose the `/intercept` routes over the admin API, build the admin server
`with_intercept(control)` where `control: InterceptControl` is the shared lifecycle slot (see
[Runtime lifecycle](#runtime-lifecycle-admin-api) below). The standalone `rift` binary always wires
one in, so those routes are available on every server.

### Standalone binary

The `rift` binary starts an intercept listener when `--intercept-port` is set; its rule store and
CA are automatically shared with the admin API, so the `/intercept/*` routes below configure it:

```bash
# Generate a CA in-memory:
rift --intercept-port 8443
# Or load an existing CA from files:
rift --intercept-port 8443 --intercept-ca-cert ca.pem --intercept-ca-key ca.key
# Or pass the CA inline as PEM (issue #593) — env is the intended vehicle:
RIFT_INTERCEPT_CA_CERT_PEM="$(cat ca.pem)" RIFT_INTERCEPT_CA_KEY_PEM="$(cat ca.key)" \
  rift --intercept-port 8443
```

(Equivalently `RIFT_INTERCEPT_PORT` / `RIFT_INTERCEPT_CA_CERT` / `RIFT_INTERCEPT_CA_KEY`, or the
inline `RIFT_INTERCEPT_CA_CERT_PEM` / `RIFT_INTERCEPT_CA_KEY_PEM`. The file pair and the PEM pair are
mutually exclusive — passing both is a startup error. There is no `returnCaKey` at launch; bootstrap
a CA over the admin API instead, so the key is never printed to logs.)

`--intercept-port` is just an *eager* start of the same listener the admin API can start at runtime
— it is no longer the only way to enable intercept. A server started **without** the flag still
serves the lifecycle endpoints below, so a client can turn intercept on later.

### Declare it in the config file

The flags above start a listener with **no rules**, so a container has to `curl` the admin API after
boot to install them — a bootstrap sidecar that exits `0` (and so crash-loops under Kubernetes'
`restartPolicy: Always`), and a window where the SUT's first calls race the rule that isn't there
yet. Instead, put an `intercept` block next to your imposters in `--configfile`: the listener binds
with its rules **already installed**, so the server is correct the moment it is ready.

```json
{
  "imposters": [
    { "port": 4545, "protocol": "http", "name": "Optimizely datafile",
      "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "{\"featureX\":\"ON\"}" } }] }] }
  ],
  "intercept": {
    "host": "0.0.0.0",
    "port": 8080,
    "caCertPath": "/certs/rift-ca-cert.pem",
    "caKeyPath": "/certs/rift-ca-key.pem",
    "rules": [
      { "host": "cdn.example.com", "action": { "forward": { "port": 4545 } } }
    ]
  }
}
```

```bash
rift --configfile /config/optimizely.json   # listener up + rules installed; no admin call, no sidecar
```

The block is the same shape as the `POST /intercept` body — `host`, `port`, the CA fields
(`caCertPath`/`caKeyPath` or inline `caCertPem`/`caKeyPem`), plus `rules[]` using the
[rule schema](#configuring-rules-admin-api) verbatim. Notes:

- **Optional and additive.** A config file without an `intercept` block behaves exactly as before.
- **It lives in the wrapper form.** Only the `{ "imposters": [...] }` object can carry an
  `intercept` block. Putting one in a single-imposter document (`{"port": 4545, ...}`) is a startup
  error naming the fix, never a silently ignored block — add `"imposters": [ ... ]` around the
  imposter, using `[]` if the file declares none. A bare top-level array, and a YAML config (which
  is the array form), have nowhere to put a block at all.
- **One source of truth.** Supplying the block *and* any `--intercept-*` flag is a startup error
  rather than a silent precedence guess. Use one or the other.
- **Runtime rules still layer on top.** `POST /intercept/rules` adds to the config-seeded set and
  `DELETE /intercept/rules` clears it; `GET` lists both.
- **`rules` works over the admin API and FFI too.** `POST /intercept` and `rift_start_intercept`
  accept the same optional `rules` array, so any surface can start-and-seed in one call.
- **Boot-only.** `POST /admin/reload` re-applies imposters only. When the reloaded file carries an
  `intercept` block, the response body carries a `warnings` entry saying it was not re-applied (and
  the server logs a warning), so an edit to the block never *looks* applied. Change rules at runtime
  over the admin API, or restart to re-read the block.
- **Injection is gated.** A rule whose predicates use `inject` needs `--allowInjection`, exactly as a
  config-file imposter's scripting surface does — the file crossed the same trust boundary.

An `intercept` block also gets EJS preprocessing like the rest of the file, so
`"host": "<%= process.env.CDN_HOST %>"` works.

### Runtime lifecycle (admin API)

Start, inspect, and stop the intercept listener at runtime over the admin API — no restart, and no
`--intercept-port` required (issue #493). This is what lets an SDK enable intercept against a server
it merely *connected* to.

```
POST   /intercept   body (optional): { "host"?: "127.0.0.1", "port"?: 0,
                                        "caCertPath"?: "...",  "caKeyPath"?: "...",
                                        "caCertPem"?: "...",   "caKeyPem"?: "...",
                                        "returnCaKey"?: false }
                    → 201 { "interceptPort": N, "interceptUrl": "http://127.0.0.1:N" }
                    → 409 if a listener is already running (flag, FFI, or a prior POST)
GET    /intercept   → 200 { "interceptPort": N, "interceptUrl": "..." }  |  404 when not running
DELETE /intercept   → 204 always (idempotent); stops the listener and drops its rules + CA
```

- The body is optional: absent, empty, or `{}` all mean defaults (`127.0.0.1:0`, a fresh in-memory
  CA). Port `0` is OS-assigned; read the real port back from the response.
- The default bind host is `127.0.0.1` — **not** the admin server's host. A containerized,
  connect-transport caller that needs the proxy reachable off-box must pass `"host": "0.0.0.0"`
  explicitly.
- **Supplying a CA.** Three mutually-exclusive options: none (a fresh CA is generated),
  `caCertPath`/`caKeyPath` (PEM **files** on the engine's filesystem), or `caCertPem`/`caKeyPem`
  (inline PEM **bytes** in the request body — issue #593). Inline PEM lets an SDK hand a
  containerized engine its CA over the admin API with no volume mount. Each pair is
  both-or-neither, and the path pair and PEM pair cannot be combined. A half-supplied pair, both
  pairs together, an unknown field, a bad CA, or an occupied port is a `400` with the standard error
  envelope; a supplied private key is never echoed back.
- **Bootstrapping a CA (`returnCaKey`).** Set `"returnCaKey": true` (only when **no** CA source is
  supplied) to have Rift mint a fresh CA and return **both** its cert and key in the `201` response,
  so you can persist and redistribute a shareable anchor instead of pre-making one with `openssl`:
  ```json
  { "interceptPort": N, "interceptUrl": "...", "caCertPem": "-----BEGIN CERTIFICATE-----…",
    "caKeyPem": "-----BEGIN PRIVATE KEY-----…" }
  ```
  The key is returned **once**, in this response only — `GET /intercept` never carries it. Combining
  `returnCaKey` with any supplied `caCert*`/`caKey*` is a `400` (it would otherwise let a caller echo
  back an arbitrary keypair from the engine's filesystem). Absent `returnCaKey`, the response carries
  no CA fields, exactly as before. **Security:** `caKeyPem` is CA private-key material — treat the
  response as a secret, transport it over the `--apikey`-gated admin plane only, and prefer a
  pre-provisioned CA where policy requires the key never transit the API.
- `DELETE` discards the CA along with the listener, so a later `POST` without a CA source mints a
  **fresh** CA — re-export `/intercept/ca.pem` (below), or bootstrap with `returnCaKey` and supply
  the pair back via `caCertPem`/`caKeyPem`, after any restart.
- All three verbs are gated by `--apikey` like every other admin route.

```bash
# Enable intercept on an already-running server, then read back the proxy port:
curl -sX POST http://localhost:2525/intercept
# {"interceptPort":49711,"interceptUrl":"http://127.0.0.1:49711"}

# ...configure rules / export the CA (see below), point the SUT at the proxy...

# Tear it down when done (safe to call unconditionally):
curl -sX DELETE http://localhost:2525/intercept
```

### Embedding over the C-ABI (non-Rust)

> A non-Rust host (JVM, Node, Go, Python, …) can start and drive the intercept listener with **no
> loopback HTTP and no Rust code** — see [FFI (C-ABI)]({{ site.baseurl }}/embedding/ffi/#intercept-proxy-over-ffi).
> `rift_start_intercept` starts the listener, `rift_stop_intercept` stops it, and the
> `rift_intercept_*` control-plane functions — `rift_intercept_add_rules`,
> `rift_intercept_list_rules`, `rift_intercept_clear_rules`, `rift_intercept_export_truststore`, and
> `rift_intercept_ca_pem` — add rules, list them, export a truststore, and fetch the CA PEM, all
> over C-ABI. The listener started this way is the *same* one `rift_serve_admin`'s `/intercept`
> routes see: `rift_start_intercept` then `GET /intercept` reports it, and a double-start across the
> two surfaces conflicts consistently (409 / `-1`).

---

## Configuring rules (admin API)

A rule matches an intercepted request by **host** (exact, case-insensitive; omit for any) and
**predicates** (the usual Mountebank predicate JSON, AND-ed), and carries one action.

Body predicates see a request body that is not valid UTF-8 (protobuf, gzip, an image upload) as
its **standard base64 encoding** (with padding) — the same convention as
[binary recorded requests]({{ site.baseurl }}/mountebank/imposters) and binary responses. Write
the predicate against the base64 string, e.g.
`{ "equals": { "body": "H4sIAAAAAAAA/w==" } }`. A valid-UTF-8 (text or JSON) body is matched
as-is, unchanged. Forwarding always relays the raw bytes regardless of classification.

> **`inject` predicates require `--allowInjection`.** A rule's predicates are evaluated on every
> intercepted request, so an `inject` predicate is executable JavaScript — the same surface
> `--allowInjection` gates on imposter stubs. Without the flag, a rule carrying one (however deeply
> nested under `not`/`or`/`and`) is refused with `400` and the whole request is rejected: a batch
> containing one such rule stores none of it. This holds on every door that admits a rule —
> `POST /intercept/rules`, the `rules` array on `POST /intercept`, and the `--configfile`
> `intercept` block. `serve` and `forward` actions carry no script and are never gated.

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
| `POST /intercept/rules` | Add one rule (object) or many (array). Rejected with `429 Too Many Requests` once the store holds 10,000 rules — `DELETE` rules before adding more. |
| `GET /intercept/rules` | List all rules |
| `DELETE /intercept/rules` | Remove all rules |

The rule store is capped at 10,000 rules to bound both memory and the per-request match scan; a
batch `POST` that would exceed the cap is rejected in full (no partial add).

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

The JKS store above is `-Djavax.net.ssl.trustStoreType=JKS` (the JVM default); a PKCS#12 store also
loads as a JVM trust anchor by adding the type:

```
-Djavax.net.ssl.trustStore=ts.p12 -Djavax.net.ssl.trustStoreType=PKCS12 \
-Djavax.net.ssl.trustStorePassword=changeit \
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
- The listener is started by an **embedder** (or the zio-bdd adapter), from the standalone `rift`
  binary via `--intercept-port` (see [Standalone binary](#standalone-binary)), or at runtime over
  the admin API (see [Runtime lifecycle](#runtime-lifecycle-admin-api)) — one listener at a time
  either way.
