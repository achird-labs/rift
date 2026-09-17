---
layout: default
title: TLS/HTTPS
parent: Features
nav_order: 3
---

# TLS/HTTPS Support

Rift speaks TLS in three places: on **HTTPS imposters** (Rift is the server), on **outbound calls**
(Rift is the client — `proxy` stubs and remote config sources), and on the **intercept proxy**
(Rift impersonates someone else's server). This page covers the first two and points to the third.

---

## Which certificate are you configuring?

Five different things here involve a certificate, and they point in opposite directions. Pick the
row that matches what you are trying to do, or what is failing:

| What is happening | Who has to trust whom | You want |
|:------------------|:----------------------|:---------|
| Rift proxies or records to a real origin, and fails with `UnknownIssuer` | **Rift** must trust the **origin's** CA | [`--upstream-ca-file`](#trusting-a-private-ca) |
| Your app calls a Rift HTTPS imposter and rejects its certificate | Your **app** must trust **Rift's** imposter certificate | [`cert`/`key` on the imposter](#custom-certificate), or trust the [generated self-signed one](#basic-https-imposter) |
| Every HTTPS imposter should serve the same certificate | Your **app** must trust that certificate | [`--default-tls-cert` / `--default-tls-key`](#server-wide-default-certificate) |
| Your app is pointed at Rift's intercept (MITM) proxy and rejects it | Your **app** must trust **Rift's intercept CA** | [Intercept proxy]({{ site.baseurl }}/features/intercept-proxy/#trusting-the-ca-from-the-sut) — `GET /intercept/ca.pem`, or the PKCS#12/JKS truststore export |
| You want Rift to demand a certificate *from* the caller | **Rift** must trust the **caller's** CA | [`mutualAuth` / `rejectUnauthorized` / `ca`](#mutual-tls-mtls) |

The first and fourth are the ones most often confused. The intercept CA is Rift's *own* certificate
authority, minted so a system under test will accept Rift standing in for someone else — it does
nothing to help Rift reach an origin behind your company's private CA. If your error is
`UnknownIssuer` on a `proxy` stub, you want the first row.

Every recipe below uses the files from [Generating Certificates](#generating-certificates):
`ca.crt`, `server.crt`/`server.key` and `client.crt`/`client.key`.

---

## HTTPS Imposters (Mountebank Mode)

### Basic HTTPS Imposter

```json
{
  "port": 4545,
  "protocol": "https",
  "stubs": [{
    "responses": [{
      "is": { "statusCode": 200, "body": "Secure response" }
    }]
  }]
}
```

With no certificate supplied, Rift generates a self-signed one, valid for `localhost` and
`127.0.0.1`. It is minted fresh when the imposter is created (and again when it is re-bound, e.g.
after a restart), so there is nothing stable to trust. Skip verification in tests that don't care:

```bash
curl -k https://localhost:4545/
# Secure response
```

To have clients verify the certificate, give the imposter one of your own.

### Which certificate an imposter serves

An HTTPS imposter picks its certificate in this order:

1. its own inline `cert` + `key`;
2. otherwise the server default, [`--default-tls-cert` + `--default-tls-key`](#server-wide-default-certificate);
3. otherwise a generated self-signed certificate — unless `--no-self-signed-tls` is set, in which
   case the imposter is refused.

Each pair is both-or-neither. A `cert` without a `key` (or the reverse) is refused rather than
falling through to the next source, and so is a half-configured server default. `cert`/`key` on an
`http` imposter are ignored, as in Mountebank.

The imposter's TLS settings are checked when it is created. Over the admin API a refusal is a
`400` whose message starts `TLS configuration error:`. An imposter loaded at startup
(`--configfile`, `--datadir`) is skipped instead, with the reason in an error-level log line, while
the rest of the server starts. An HTTPS imposter never quietly falls back to cleartext.
TLS 1.2 and 1.3 are offered; older versions are not.

### Custom Certificate

```json
{
  "port": 4545,
  "protocol": "https",
  "key": "-----BEGIN PRIVATE KEY-----\nMIIE...\n-----END PRIVATE KEY-----",
  "cert": "-----BEGIN CERTIFICATE-----\nMIID...\n-----END CERTIFICATE-----",
  "stubs": [...]
}
```

- `key` may be PKCS#8 (`BEGIN PRIVATE KEY`), PKCS#1 RSA (`BEGIN RSA PRIVATE KEY`) or SEC1 EC
  (`BEGIN EC PRIVATE KEY`).
- `cert` may hold a chain — leaf first, then intermediates — and the whole chain is served.
- A key that does not match the certificate is refused (`cert/key mismatch?`).

PEM has to be JSON-escaped to go in a string. `jq` does it for you, so you can create the imposter
straight from the files:

```bash
jq -n --rawfile cert server.crt --rawfile key server.key '{
  port: 4545, protocol: "https", cert: $cert, key: $key,
  stubs: [{ responses: [{ is: { statusCode: 200, body: "Secure response" } }] }]
}' | curl -s -X POST http://localhost:2525/imposters \
       -H 'Content-Type: application/json' --data-binary @-

curl --cacert ca.crt https://localhost:4545/
# Secure response
```

Without `--cacert` the same call fails with `curl: (60) SSL certificate problem`, because `ca.crt`
is not in the system trust store.

> The inline `key` is part of the imposter's definition, so it is written to `--datadir` and
> returned by `GET /imposters?replayable=true` — treat both as secrets. `GET /imposters/:port` does
> not include it. To keep the key out of the imposter entirely, use a
> [server-wide default](#server-wide-default-certificate) instead.

### Certificate from Files (EJS)

```json
{
  "port": 4545,
  "protocol": "https",
  "key": "<%- stringify('/path/to/server.key') %>",
  "cert": "<%- stringify('/path/to/server.crt') %>",
  "stubs": [...]
}
```

`stringify` escapes the file for use inside a JSON string, so the PEM's line breaks survive.
`<% include %>` inlines the file as raw text, which breaks the string. A relative path is resolved
against the config file's directory. Both tags work only in a local file, `--configfile` or a
`file:` source. A document from any other source is refused with an error naming the tag, and
`--datadir` files are not preprocessed at all.

### Server-wide default certificate

Give every HTTPS imposter that has no `cert`/`key` of its own the same certificate:

```bash
rift --default-tls-cert server.crt --default-tls-key server.key --no-self-signed-tls
# or
RIFT_DEFAULT_TLS_CERT=server.crt RIFT_DEFAULT_TLS_KEY=server.key RIFT_NO_SELF_SIGNED_TLS=true rift
```

```bash
curl -s -X POST http://localhost:2525/imposters -d '{
  "port": 4545, "protocol": "https",
  "stubs": [{ "responses": [{ "is": { "body": "Secure response" } }] }]
}'
curl --cacert ca.crt https://localhost:4545/
# Secure response
```

The files are read once, at startup; a missing file stops startup. Only one of the two flags set is
not caught then: every HTTPS imposter that would have used the default is refused with
`server default TLS must provide both cert and key, or neither`.

`--no-self-signed-tls` is optional. It turns an HTTPS imposter with no certificate from any source
into an error (`no cert/key provided and self-signed generation is disabled`) instead of a
generated certificate your clients will not trust. The demo in
[`docs/demo`](https://github.com/achird-labs/rift/tree/master/docs/demo#demo-2-httpstls-mode) uses the default-certificate flags.

### Mutual TLS (mTLS)

Three fields, matching Mountebank's grammar:

| Field | Meaning |
|:------|:--------|
| `mutualAuth` | Request **and require** a client certificate. `https` only. |
| `rejectUnauthorized` | Validate the client certificate against `ca`. Requires `ca`. |
| `ca` | PEM trust anchor(s) client certificates must chain to. A string or an array. |

**Require a certificate, without validating it.** Useful for virtualizing a server that demands
mutual auth when you do not have that server's PKI. Any certificate the client holds the key for
is accepted (the handshake signature is still checked, so a certificate copied off the wire is
not enough). A client presenting nothing fails the handshake:

```json
{
  "port": 4545,
  "protocol": "https",
  "cert": "-----BEGIN CERTIFICATE-----\n...",
  "key": "-----BEGIN PRIVATE KEY-----\n...",
  "mutualAuth": true,
  "stubs": [...]
}
```

**Require and validate**, against your own CA:

```json
{
  "port": 4545,
  "protocol": "https",
  "mutualAuth": true,
  "rejectUnauthorized": true,
  "ca": "-----BEGIN CERTIFICATE-----\n...",
  "stubs": [...]
}
```

`cert`/`key` are optional here as everywhere: omit them and the imposter serves its
[usual certificate](#which-certificate-an-imposter-serves), with client auth still enforced.

#### Walkthrough: require and validate a client certificate

```bash
# 1. Create the imposter. The server certificate and the client-auth CA are both from
#    "Generating Certificates" below; they happen to share ca.crt, but need not.
jq -n --rawfile cert server.crt --rawfile key server.key --rawfile ca ca.crt '{
  port: 4545, protocol: "https", cert: $cert, key: $key,
  mutualAuth: true, rejectUnauthorized: true, ca: $ca,
  stubs: [{ responses: [{ is: { statusCode: 200, body: "hello, client" } }] }]
}' | curl -s -X POST http://localhost:2525/imposters \
       -H 'Content-Type: application/json' --data-binary @-

# 2. With a client certificate that chains to `ca`:
curl --cacert ca.crt --cert client.crt --key client.key https://localhost:4545/
# hello, client

# 3. Without one — the handshake is refused, no HTTP status is ever sent:
curl --cacert ca.crt https://localhost:4545/
# curl: (56) ... alert certificate required
#   (macOS's LibreSSL curl prints the numeric form: ... reason(1116))
```

A client certificate issued by some other CA fails the same way; curl reports a
`certificate unknown` alert. With `mutualAuth` alone (no `rejectUnauthorized`), step 3
still fails, but any client certificate, from any issuer, gets through step 2.

A client certificate needs no special extensions. If it has an Extended Key Usage, that must
include `clientAuth` — a server-only certificate (`serverAuth` alone) is rejected. The client's
identity is not passed to predicates or recorded requests: it gates the connection, nothing more.

Every combination that cannot take effect is refused at creation with a `400`, rather than accepted
and quietly ignored:

| Config | Why it is refused |
|:-------|:------------------|
| `rejectUnauthorized` or `ca` without `mutualAuth` | No client certificate is ever requested, so neither can apply. |
| `ca` without `rejectUnauthorized` | The certificate is required but its chain is never validated — the CA you supplied would be ignored. |
| `rejectUnauthorized` without `ca` (or `ca: []`) | There is nothing to validate against. |
| `mutualAuth: true` on `protocol: "http"` | A cleartext listener cannot request a certificate. |
| `ca` containing no certificate (e.g. a private key by mistake) | It cannot serve as a trust anchor. |

`"mutualAuth": false` on an `http` imposter stays valid, so existing configs keep working.

This is stricter than Mountebank, which accepts all of the above and silently does nothing with
them. That silence is the bug this feature was filed to fix. If a security setting is accepted and
reads back unchanged but does nothing, nobody finds out; a rejection at least tells the author.
Rift answers the `POST /imposters` with the reason instead.

> **Divergence from Mountebank.** There, `mutualAuth` only *requests* a certificate and never
> rejects one — and because its implementation gates the request on `rejectUnauthorized`, a bare
> `mutualAuth: true` does nothing at all. Rift requires the certificate, because a mock whose job is
> to stand in for an mTLS gateway should fail a client that forgot one.

---

## HTTP/2 over TLS

HTTPS imposters offer `h2` and `http/1.1` via ALPN, so an HTTP/2 client gets HTTP/2 with no
configuration:

```bash
curl -v --http2 --cacert ca.crt https://localhost:4545/ 2>&1 | grep -i alpn
# * ALPN: server accepted h2
```

An imposter that can fire a TCP fault or runs a `_rift.script` response, and every imposter under
`RIFT_DISABLE_HTTP2=1`, offers only `http/1.1` — so a client that offers **only** `h2` is refused at
the handshake (`no_application_protocol`) rather than handed a protocol the imposter will not
speak. See [HTTP/2 and h2c]({{ site.baseurl }}/mountebank/imposters/#http2-and-h2c) for the full
rules. The intercept listener follows the same switch.

---

## TLS Performance

### Session resumption

Mock servers see bursts of handshakes: load generators and test suites open many short-lived
TLS connections rather than a few long-lived ones. A **resumed** handshake skips the expensive
asymmetric crypto of a full handshake, so resumption does more for a mock's TLS throughput than
anything else.

Every HTTPS imposter (custom-cert and auto-generated self-signed alike) and the intercept listener
are configured for resumption out of the box — no configuration is required:

- a sized in-memory **session cache** (TLS 1.2 session IDs and TLS 1.3 stateful resumption), and
- a **session ticketer** for stateless resumption (TLS 1.3 tickets and TLS 1.2 RFC 5077) — the client
  presents a ticket on reconnect. Its encryption key auto-rotates roughly every 6 hours.

A client that reuses its TLS session (most HTTP clients and load generators do by default) will
resume on reconnect. You can confirm resumption with `openssl s_client`:

```bash
# -reconnect performs 5 handshakes reusing the session; look for "Reused" on the later ones
openssl s_client -connect localhost:4545 -reconnect 2>/dev/null | grep -E "New|Reused"
```

### Crypto provider

Rift pins the **`ring`** rustls crypto provider. `aws-lc-rs` (rustls' newer default) is faster at
bulk transfer, but its C build fails on the windows-msvc CI runner and breaks the FFI
cross-compile matrix, so it is not a portable option. A mock serves small responses, so handshake
rate matters far more than bulk throughput, and there `ring` holds its own. The provider is not
user-configurable.

---

## HTTPS Proxy

### Proxy to HTTPS Backend

```json
{
  "stubs": [{
    "responses": [{
      "proxy": {
        "to": "https://api.example.com"
      }
    }]
  }]
}
```

The origin's certificate is verified against the operating system trust store. Outbound
connections offer `http/1.1` only, so an origin that accepts nothing but HTTP/2 cannot be proxied.

### Trusting a Private CA

Rift verifies outbound TLS against the operating system trust store. An origin issued by an
internal CA — a corporate API gateway — needs that CA supplied:

```bash
rift --upstream-ca-file /etc/rift/corp-ca.pem
# or
RIFT_UPSTREAM_CA_FILE=/etc/rift/corp-ca.pem rift
```

The anchor is **appended** to the OS store, so public origins keep working. The file may hold
several certificates. It is read and checked at startup, so a missing file, or one containing no
usable certificate, stops the server there rather than failing the first proxied request.

The standalone binary uses this trust for every outbound TLS connection it makes: `proxy` stub
upstreams, `--configfile https://…`, and the intercept listener's
[WebSocket passthrough]({{ site.baseurl }}/features/intercept-proxy/#websocket-passthrough).
Embedders set the same policy on `rift_serve_admin` with `upstreamCaFile` (a path), `upstreamCaPem`
(the PEM text; not both) and `upstreamTlsSkipVerify` — see
[FFI]({{ site.baseurl }}/embedding/ffi/).

To check the setup end to end, proxy to an origin that only your CA vouches for:

```bash
rift --upstream-ca-file ca.crt &
curl -s -X POST http://localhost:2525/imposters -d '{
  "port": 4546, "protocol": "http",
  "stubs": [{ "responses": [{ "proxy": { "to": "https://localhost:4545" } }] }]
}'
curl http://localhost:4546/    # answered by the HTTPS imposter from "Custom Certificate"
```

Without `--upstream-ca-file` the last call is answered `502` with an `x-rift-proxy-error: true`
header and a `Proxy error: Failed to send proxy request to https://localhost:4545/` body. The
underlying cause, `invalid peer certificate: UnknownIssuer`, is written to the server log only, so
a response never leaks the TLS error chain to the caller.

> `SSL_CERT_FILE` / `SSL_CERT_DIR` are also honoured, but they **replace** the trust store rather
> than adding to it — pointing `SSL_CERT_FILE` at a lone private CA silently drops every public
> root. Use `--upstream-ca-file` unless you are supplying a complete bundle.

### Skipping Verification (development only)

```bash
rift --upstream-tls-skip-verify      # or RIFT_UPSTREAM_TLS_SKIP_VERIFY=true
```

Accepts any certificate and logs a warning. If `--upstream-ca-file` is also set, that CA is not
used for anything, and a second warning says so. Prefer `--upstream-ca-file`: a recording proxy with verification
disabled will faithfully record MITM'd traffic.

> `key`, `cert`, `passphrase` and `ciphers` on a `proxy` response are accepted for Mountebank
> compatibility and **are not honoured** — they are dropped on load and do not appear when the
> imposter is read back. Rift cannot present a client certificate to an upstream, and its outbound
> trust is process-wide, configured by the two flags above.

---

## Generating Certificates

One small PKI covers every recipe on this page: a CA, a server certificate signed by it, and a
client certificate signed by it. Tested with OpenSSL 3.

```bash
# CA
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout ca.key -out ca.crt -subj "/CN=Test CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

# Server certificate for localhost / 127.0.0.1
openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=localhost"
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days 365 -out server.crt -extfile server.ext

# Client certificate (for mutual TLS)
openssl req -newkey rsa:2048 -nodes -keyout client.key -out client.csr -subj "/CN=test-client"
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days 365 -out client.crt -extfile client.ext

# Check
openssl verify -CAfile ca.crt -purpose sslserver server.crt   # server.crt: OK
openssl verify -CAfile ca.crt -purpose sslclient client.crt   # client.crt: OK
```

Put every hostname clients will use in `subjectAltName` — for a container, add its service name
(`DNS:rift`). Modern TLS clients, rustls and Go included, ignore the `CN` and match only the SAN.
A bare `openssl req -x509 -subj "/CN=localhost"` certificate has no SAN, so those clients reject it.

To serve a certificate chain, concatenate it leaf first: `cat server.crt intermediate.crt > chain.crt`.

---

## Docker with TLS

### Mount Certificates

```yaml
services:
  rift:
    image: zainalpour/rift-proxy:latest
    ports:
      - "2525:2525"
      - "4545:4545"
    volumes:
      - ./certs:/certs:ro
      - ./imposters.json:/imposters.json:ro
    command: ["--configfile", "/imposters.json",
              "--default-tls-cert", "/certs/server.crt",
              "--default-tls-key", "/certs/server.key"]
```

The image runs as an unprivileged user (uid 1000), so the mounted key must be readable by it.
`openssl` writes keys `0600`, owned by you. On Linux that fails with `Permission denied (os error 13)`.
`chmod 644` is fine for throwaway test certificates, but not for real ones. The
[HTTPS demo](https://github.com/achird-labs/rift/tree/master/docs/demo#demo-2-httpstls-mode) is a working version of this setup.

### Imposter Configuration

With the default-certificate flags above, the imposter only needs `"protocol": "https"`. To give
one imposter its own certificate instead, read the files in with EJS:

```json
{
  "imposters": [{
    "port": 4545,
    "protocol": "https",
    "key": "<%- stringify('/certs/server.key') %>",
    "cert": "<%- stringify('/certs/server.crt') %>",
    "stubs": [...]
  }]
}
```

---

## Kubernetes with TLS

### Secret for Certificates

```bash
kubectl create secret tls rift-tls --cert=server.crt --key=server.key
```

This creates a `kubernetes.io/tls` Secret with the keys `tls.crt` and `tls.key`.

### Pod Configuration

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: rift
spec:
  containers:
    - name: rift
      image: zainalpour/rift-proxy:latest
      args: ["--configfile", "/config/imposters.json",
             "--default-tls-cert", "/certs/tls.crt",
             "--default-tls-key", "/certs/tls.key"]
      volumeMounts:
        - name: tls
          mountPath: /certs
          readOnly: true
        - name: config
          mountPath: /config
  volumes:
    - name: tls
      secret:
        secretName: rift-tls
    - name: config
      configMap:
        name: rift-config
```

---

## Troubleshooting

### Certificate Errors

```bash
# Inspect a certificate (check the SAN and the dates)
openssl x509 -in server.crt -noout -text

# See what an imposter actually serves
openssl s_client -connect localhost:4545 -servername localhost </dev/null

# Does it ask for a client certificate? Either "Acceptable client certificate CA names" or
# "No client certificate CA names sent" in the output means it did.
openssl s_client -connect localhost:4545 -cert client.crt -key client.key </dev/null

# Verify a chain
openssl verify -CAfile ca.crt server.crt
```

### Common Issues

| Error | Cause | Solution |
|:------|:------|:---------|
| `502` + `x-rift-proxy-error` from a `proxy` stub; log shows `invalid peer certificate: UnknownIssuer` | Origin's CA is not in the OS trust store | `--upstream-ca-file <pem>`, or `--upstream-tls-skip-verify` in development |
| `curl: (60) SSL certificate problem` calling an imposter | Your client does not trust the imposter's certificate | `--cacert ca.crt`, or `-k` for the generated self-signed certificate |
| `curl: (56) ... alert certificate required` (LibreSSL: `reason(1116)`) | The imposter has `mutualAuth` and the client sent no certificate | `--cert client.crt --key client.key` |
| `... alert certificate unknown` | The client certificate does not chain to the imposter's `ca` | Issue it from a CA listed in `ca`, or add that CA |
| `certificate has expired` | Expired cert | Regenerate certificate |
| hostname mismatch / `NotValidForName` | The name you call is not in the certificate's SAN | Add it to `subjectAltName` |
| `TLS configuration error: https imposter must provide both cert and key, or neither` | Only one of `cert`/`key` set | Supply both, or neither |
| `TLS configuration error: Failed to parse ...` / `No private key found in key PEM` | `key` is not PEM, or JSON escaping broke its line breaks | Build the body with `jq --rawfile` or EJS `stringify` |
| `TLS configuration error: ... (cert/key mismatch?)` | `key` does not belong to `cert` | Pair the key with its certificate |
| `TLS configuration error: ... self-signed generation is disabled` | `--no-self-signed-tls` and no certificate from any source | Add `cert`/`key`, or `--default-tls-cert`/`--default-tls-key` |
| `Permission denied (os error 13)` at startup in Docker | Mounted key not readable by uid 1000 | See [Docker with TLS](#docker-with-tls) |
