---
layout: default
title: Deployment
nav_order: 6
has_children: true
permalink: /deployment/
---

# Deployment

Rift can be deployed in various environments, from local development to production Kubernetes clusters.

---

## Deployment Options

### Docker (Recommended)

Quick setup for development and testing:

```bash
docker pull zainalpour/rift-proxy:latest
docker run -p 2525:2525 zainalpour/rift-proxy:latest
```

[Full Docker Guide]({{ site.baseurl }}/deployment/docker/)

### Kubernetes

Production deployment with proper resource management:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rift
spec:
  replicas: 3
  template:
    spec:
      containers:
        - name: rift
          image: zainalpour/rift-proxy:latest
          ports:
            - containerPort: 2525
            - containerPort: 9090
```

[Full Kubernetes Guide]({{ site.baseurl }}/deployment/kubernetes/)

### Binary

Standalone deployment without containers:

```bash
# Download
curl -L https://github.com/achird-labs/rift/releases/latest/download/rift-http-proxy-linux-x86_64 -o rift

# Run
chmod +x rift
./rift --configfile imposters.json
```

---

## Deployment Patterns

### Standalone Mock Server

Single Rift instance serving all imposters:

```
┌─────────────┐     ┌─────────────┐
│  Test Suite │────▶│    Rift     │
└─────────────┘     │  (imposters)│
                    └─────────────┘
```

### Sidecar Pattern

One Rift per service for isolated fault injection:

```
┌─────────────────────────────┐
│           Pod               │
│  ┌─────────┐  ┌─────────┐  │
│  │  Rift   │◀─│   App   │  │
│  │(sidecar)│  │         │  │
│  └────┬────┘  └─────────┘  │
│       │                     │
│       ▼                     │
│  ┌─────────┐               │
│  │ Backend │               │
│  └─────────┘               │
└─────────────────────────────┘
```

### API Gateway Pattern

Rift as a reverse proxy routing to multiple services:

```
                    ┌─────────────┐
┌─────────┐        │    Rift     │        ┌─────────┐
│ Client  │───────▶│  (gateway)  │───────▶│Service A│
└─────────┘        │             │        └─────────┘
                   │             │        ┌─────────┐
                   │             │───────▶│Service B│
                   └─────────────┘        └─────────┘
```

---

## Reaching an Origin Behind a Private CA

A `proxy` stub that records from a real upstream has to trust that upstream's certificate. In a
container that usually fails, because the image ships only public roots — the symptom is:

```
WARN rift_mock_core::imposter::handler: Proxy request failed: ...
     client error (Connect): invalid peer certificate: UnknownIssuer
```

Mount the CA and point Rift at it. Nothing else about the image changes, and no custom build is
needed:

```bash
docker run \
  -v $(pwd)/corp-ca.pem:/certs/corp-ca.pem:ro \
  -v $(pwd)/imposters.json:/imposters.json \
  -e RIFT_UPSTREAM_CA_FILE=/certs/corp-ca.pem \
  zainalpour/rift-proxy:latest --configfile /imposters.json
```

Kubernetes — the CA as a `Secret` or `ConfigMap`:

```yaml
        - name: rift
          image: zainalpour/rift-proxy:latest
          env:
            - name: RIFT_UPSTREAM_CA_FILE
              value: /certs/corp-ca.pem
          volumeMounts:
            - name: corp-ca
              mountPath: /certs
              readOnly: true
      volumes:
        - name: corp-ca
          configMap:
            name: corp-ca            # key: corp-ca.pem
```

Three things worth knowing:

- **It appends.** The mounted CA is added to the image's trust store, so public origins keep
  working. `SSL_CERT_FILE` is also honoured but *replaces* the store — point it at a lone private
  CA and every public root disappears.
- **This works on the `-static` image too.** It carries a CA bundle and reads it at runtime, so the
  only thing it is missing is your private CA — which is exactly what the mount supplies. Building a
  custom image is not necessary.
- **Development only:** `--upstream-tls-skip-verify` / `RIFT_UPSTREAM_TLS_SKIP_VERIFY` skips
  verification entirely. A recording proxy with verification off will faithfully record MITM'd
  traffic, so prefer the CA mount anywhere the recordings matter.

See [TLS/HTTPS]({{ site.baseurl }}/features/tls/) for the full picture, including which certificate
belongs to which direction.

---

## Environment Configuration

### Required Settings

| Setting | Description | Default |
|:--------|:------------|:--------|
| `MB_PORT` | Admin API port | `2525` |

### Optional Settings

| Setting | Description | Default |
|:--------|:------------|:--------|
| `MB_ALLOW_INJECTION` | Enable JavaScript | `false` |
| `RUST_LOG` | Log level | `info` |
| `RIFT_METRICS_PORT` | Metrics port | `9090` |
| `RIFT_UPSTREAM_CA_FILE` | PEM CA file trusted for outbound TLS — proxy stubs and `--configfile` URLs. Appended to the image's trust store | |
| `RIFT_UPSTREAM_TLS_SKIP_VERIFY` | Skip outbound certificate verification (development only) | `false` |

---

## Resource Requirements

### Minimum (Development)

- **CPU**: 0.5 cores
- **Memory**: 128MB
- **Storage**: 50MB (image)

### Recommended (Production)

- **CPU**: 2 cores
- **Memory**: 512MB
- **Storage**: 100MB

### High Throughput

- **CPU**: 4+ cores
- **Memory**: 1GB+
- **Storage**: 100MB

---

## Health Checks

### Admin API

```bash
curl http://localhost:2525/
```

### Metrics Endpoint

```bash
curl http://localhost:9090/metrics
```

### Kubernetes Probes

```yaml
livenessProbe:
  httpGet:
    path: /
    port: 2525
  initialDelaySeconds: 5
  periodSeconds: 10

readinessProbe:
  httpGet:
    path: /
    port: 2525
  initialDelaySeconds: 5
  periodSeconds: 5
```
