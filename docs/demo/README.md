---
layout: default
title: Demos
nav_order: 9.5
permalink: /demo/
---

# Rift Demo

Quick-start demos for Rift in different modes.

> **Note**: These demos use a locally-built Docker image. Run `docker build -t rift-proxy:local -f crates/rift-http-proxy/Dockerfile .` from the project root first.

## Demo 1: Mountebank Mode (HTTP)

The primary way to use Rift - Mountebank-compatible mock server.

### Start

```bash
docker compose up -d
```

### Test

```bash
# Health check
curl http://localhost:4545/health

# List users
curl http://localhost:4545/api/users

# Get single user
curl http://localhost:4545/api/users/1

# Create user
curl -X POST http://localhost:4545/api/users \
  -H "Content-Type: application/json" \
  -d '{"name": "Charlie"}'

# Test slow endpoint (2s delay)
time curl http://localhost:4545/api/slow

# Test error endpoint
curl http://localhost:4545/api/error

# Order API (different port)
curl http://localhost:4546/api/orders
```

### Manage Imposters

```bash
# List imposters
curl http://localhost:2525/imposters

# Get imposter details
curl http://localhost:2525/imposters/4545

# View recorded requests
curl http://localhost:2525/imposters/4545 | jq '.requests'

# Add new stub dynamically
curl -X POST http://localhost:2525/imposters/4545/stubs \
  -H "Content-Type: application/json" \
  -d '{
    "stub": {
      "predicates": [{ "equals": { "path": "/api/new" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "New endpoint" } }]
    }
  }'

# Delete imposter
curl -X DELETE http://localhost:2525/imposters/4545
```

### Cleanup

```bash
docker compose down
```

---

## Demo 2: HTTPS/TLS Mode

Demonstrates Rift's TLS support with custom certificates.

### Prerequisites

Generate self-signed certificates:

```bash
./generate-certs.sh
```

### Start

```bash
docker compose -f docker-compose-https.yml up -d
```

### Test

```bash
# Basic HTTPS request (with CA certificate)
curl --cacert certs/ca.crt https://localhost:4545/api/test

# Or skip verification (development only)
curl -k https://localhost:4545/api/test

# Slow endpoint (500-1000ms latency fault over TLS)
time curl --cacert certs/ca.crt https://localhost:4545/api/slow

# Flaky endpoint (30% chance of a 503 error fault)
curl --cacert certs/ca.crt https://localhost:4545/api/flaky

# Stateful counter via JavaScript inject
curl --cacert certs/ca.crt https://localhost:4545/api/counter

# View metrics (HTTP)
curl http://localhost:9091/metrics | grep rift
```

### Trust the CA (Optional)

To avoid using `--cacert` or `-k`:

**macOS:**
```bash
sudo security add-trusted-cert -d -r trustRoot \
  -k /Library/Keychains/System.keychain certs/ca.crt
```

**Linux:**
```bash
sudo cp certs/ca.crt /usr/local/share/ca-certificates/rift-demo.crt
sudo update-ca-certificates
```

### Cleanup

```bash
docker compose -f docker-compose-https.yml down
rm -rf certs/  # Optional: remove generated certificates
```

---

## Demo 3: Rift-Only Features (Fault Injection)

Demonstrates Rift's probabilistic fault injection via the `_rift.fault` extension - not available in Mountebank.

### Start

```bash
docker compose -f docker-compose-rift-features.yml up -d
```

### Test Fault Injection (Port 4547)

```bash
# Healthy baseline endpoint (no faults)
curl http://localhost:4547/api/healthy

# Random latency injection (500-2000ms, 100% probability)
time curl http://localhost:4547/api/slow-random

# Probabilistic latency (50% chance of 1s delay)
time curl http://localhost:4547/api/sometimes-slow

# Error injection (30% chance of 503)
for i in {1..5}; do curl -w " [%{http_code}]\n" http://localhost:4547/api/flaky; done

# Combined chaos (70% latency + 20% errors)
time curl -w " [%{http_code}]\n" http://localhost:4547/api/chaos

# TCP connection reset
curl http://localhost:4547/api/tcp-reset
```

### Cleanup

```bash
docker compose -f docker-compose-rift-features.yml down
```

---

## Demo 4: Scripting with Flow State

Demonstrates Rift's Rhai scripting engine with persistent flow state for stateful mock scenarios.

### Start

```bash
docker compose -f docker-compose-scripting.yml up -d
```

### Test Counter API (Port 4550)

```bash
# Get counter (initial value)
curl http://localhost:4550/api/counter

# Increment counter
curl -X POST http://localhost:4550/api/counter/increment
curl -X POST http://localhost:4550/api/counter/increment

# Get counter (should be 2)
curl http://localhost:4550/api/counter

# Reset counter
curl -X DELETE http://localhost:4550/api/counter
```

### Test Rate Limiter

```bash
# First 5 requests succeed with remaining count
for i in {1..5}; do curl http://localhost:4550/api/rate-limited; echo; done

# 6th+ requests return 429 Too Many Requests
curl http://localhost:4550/api/rate-limited

# Reset rate limiter
curl -X DELETE http://localhost:4550/api/rate-limited/reset
```

### Test Echo with Count

```bash
# Each request echoes back method/path with incrementing count
curl -X POST http://localhost:4550/api/echo
curl -X POST http://localhost:4550/api/echo
```

### Cleanup

```bash
docker compose -f docker-compose-scripting.yml down
```

---

## Demo 5: Multi-Engine Scripting

Demonstrates both scripting engines (Rhai, JavaScript) with equivalent functionality.

### Start

```bash
# Using local binary
./target/release/rift --configfile docs/demo/imposters-scripting-engines.json

# Or using Docker
docker run -p 2525:2525 -p 4560:4560 \
  -v $(pwd)/docs/demo/imposters-scripting-engines.json:/imposters.json:ro \
  zainalpour/rift-proxy:latest --configfile /imposters.json
```

### Test All Engines (Port 4560)

```bash
# Health check - lists available engines
curl http://localhost:4560/health

# Rhai engine - counter with ctx.state
curl http://localhost:4560/rhai/counter
curl http://localhost:4560/rhai/counter
curl -X POST http://localhost:4560/rhai/echo

# JavaScript engine - counter with state (Mountebank inject format)
curl http://localhost:4560/js/counter
curl http://localhost:4560/js/counter
curl -X POST http://localhost:4560/js/echo
```

### Scripting Format Differences

| Engine | Format | State Access | Request Access |
|:-------|:-------|:-------------|:---------------|
| Rhai | `_rift.script` (v2 `ctx`) | `ctx.state.get(key)` | `ctx.request.method`, `ctx.request.path` |
| JavaScript | `_rift.script` (v2 `ctx`), or `inject` (Mountebank) | `ctx.state.get(key)` | `ctx.request.method` |

---

## Configuration Files

| File | Description |
|:-----|:------------|
| `imposters.json` | Mountebank HTTP imposter config |
| `imposters-rift-features.json` | Fault injection demo config |
| `imposters-scripting.json` | Scripting with flow state demo config |
| `imposters-scripting-engines.json` | Multi-engine scripting demo (Rhai, JS) |
| `docker-compose.yml` | HTTP demo |
| `docker-compose-https.yml` | HTTPS/TLS demo |
| `docker-compose-rift-features.yml` | Fault injection demo |
| `docker-compose-scripting.yml` | Scripting with flow state demo |
| `generate-certs.sh` | Certificate generation script |

---

## Rift Extensions (`_rift` namespace)

Rift extends Mountebank with advanced features through the `_rift` namespace:

- **Flow State**: Stateful testing with in-memory or Redis backends
- **Fault Injection**: Probabilistic latency, error, and TCP faults
- **Scripting**: Multi-engine scripting (Rhai, JavaScript)

Example imposter with `_rift` extensions:

```json
{
  "port": 4545,
  "protocol": "http",
  "_rift": {
    "flowState": {"backend": "inmemory", "ttlSeconds": 300}
  },
  "stubs": [{
    "predicates": [{"equals": {"path": "/api/test"}}],
    "responses": [{
      "is": {"statusCode": 200, "body": "OK"},
      "_rift": {
        "fault": {
          "latency": {"probability": 0.3, "minMs": 100, "maxMs": 500}
        }
      }
    }]
  }]
}
```

See the [Rift Extensions documentation](/docs/features/rift-extensions.md) for more details.
