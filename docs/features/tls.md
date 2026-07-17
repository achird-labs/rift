---
layout: default
title: TLS/HTTPS
parent: Features
nav_order: 3
---

# TLS/HTTPS Support

Rift supports HTTPS for both listening and upstream connections.

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

Rift generates a self-signed certificate automatically.

### Custom Certificate

```json
{
  "port": 4545,
  "protocol": "https",
  "key": "-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n-----END RSA PRIVATE KEY-----",
  "cert": "-----BEGIN CERTIFICATE-----\nMIID...\n-----END CERTIFICATE-----",
  "stubs": [...]
}
```

### Certificate from Files (EJS)

```json
{
  "port": 4545,
  "protocol": "https",
  "key": "<%- include('/path/to/server.key') %>",
  "cert": "<%- include('/path/to/server.crt') %>",
  "stubs": [...]
}
```

### Mutual TLS (mTLS)

Require client certificate:

```json
{
  "port": 4545,
  "protocol": "https",
  "key": "-----BEGIN RSA PRIVATE KEY-----\n...",
  "cert": "-----BEGIN CERTIFICATE-----\n...",
  "mutualAuth": true,
  "stubs": [...]
}
```

---

## TLS Performance

### Session resumption

Mock-server load is *handshake-storm-shaped*: load generators and test suites open many short-lived
TLS connections rather than a few long-lived ones. A **resumed** handshake skips the expensive
asymmetric crypto of a full handshake, so resumption is the dominant TLS throughput lever for a mock.

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

Rift pins the **`ring`** rustls crypto provider. `aws-lc-rs` (rustls' newer default) has a bulk-transfer
edge but its C build fails on the windows-msvc CI runner and breaks the FFI cross-compile matrix, so it
is not a portable option — and for the small responses a mock serves, handshake rate (where `ring` is
competitive) matters far more than bulk throughput. The provider is not user-configurable.

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

### Skip Certificate Verification

For self-signed certificates in development:

```json
{
  "proxy": {
    "to": "https://internal-service.local",
    "cert": null
  }
}
```

### Proxy with Client Certificate

```json
{
  "proxy": {
    "to": "https://mtls-service.example.com",
    "key": "-----BEGIN RSA PRIVATE KEY-----\n...",
    "cert": "-----BEGIN CERTIFICATE-----\n..."
  }
}
```

---

## Generating Certificates

### Self-Signed Certificate

```bash
# Generate private key
openssl genrsa -out server.key 2048

# Generate certificate
openssl req -new -x509 -key server.key -out server.crt -days 365 \
  -subj "/CN=localhost"
```

### With Subject Alternative Names

```bash
# Create config file
cat > san.cnf << EOF
[req]
distinguished_name = req_distinguished_name
req_extensions = v3_req
prompt = no

[req_distinguished_name]
CN = localhost

[v3_req]
subjectAltName = @alt_names

[alt_names]
DNS.1 = localhost
DNS.2 = *.local
IP.1 = 127.0.0.1
EOF

# Generate certificate
openssl req -new -x509 -key server.key -out server.crt -days 365 \
  -config san.cnf -extensions v3_req
```

### CA-Signed Certificate

```bash
# Generate CA
openssl genrsa -out ca.key 4096
openssl req -new -x509 -key ca.key -out ca.crt -days 3650 \
  -subj "/CN=Test CA"

# Generate server key and CSR
openssl genrsa -out server.key 2048
openssl req -new -key server.key -out server.csr \
  -subj "/CN=localhost"

# Sign with CA
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key \
  -CAcreateserial -out server.crt -days 365
```

---

## Docker with TLS

### Mount Certificates

```yaml
version: '3.8'
services:
  rift:
    image: zainalpour/rift-proxy:latest
    ports:
      - "2525:2525"
      - "4545:4545"
    volumes:
      - ./certs:/certs:ro
      - ./imposters.json:/imposters.json
    command: ["--configfile", "/imposters.json"]
```

### Imposter Configuration

```json
{
  "imposters": [{
    "port": 4545,
    "protocol": "https",
    "key": "<%- include('/certs/server.key') %>",
    "cert": "<%- include('/certs/server.crt') %>",
    "stubs": [...]
  }]
}
```

---

## Kubernetes with TLS

### Secret for Certificates

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: rift-tls
type: kubernetes.io/tls
data:
  tls.crt: <base64-encoded-cert>
  tls.key: <base64-encoded-key>
```

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
# Verify certificate
openssl x509 -in server.crt -text -noout

# Test connection
openssl s_client -connect localhost:4545

# Verify certificate chain
openssl verify -CAfile ca.crt server.crt
```

### Common Issues

| Error | Cause | Solution |
|:------|:------|:---------|
| `certificate verify failed` | Self-signed cert | Use `verify: false` or add CA |
| `certificate has expired` | Expired cert | Regenerate certificate |
| `hostname mismatch` | Wrong CN/SAN | Include correct hostname in cert |
| `no suitable key` | Wrong key format | Convert to PEM format |
