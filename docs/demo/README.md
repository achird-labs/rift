---
layout: default
title: Demos
nav_order: 9.5
permalink: /demo/
---

# Rift Demo

Quick-start demos for Rift in different modes.

> **Nothing to build.** These demos pull the published `zainalpour/rift-proxy:latest` image, so
> `docker compose up -d` works from a fresh clone. To run one against your own working tree instead,
> build and retag it first:
>
> ```bash
> docker build -t rift-proxy:local -f crates/rift-http-proxy/Dockerfile .
> docker tag rift-proxy:local zainalpour/rift-proxy:latest
> ```

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

Demonstrates Rift's TLS support with your own certificate.

### Prerequisites

Generate a throwaway demo CA and a server certificate signed by it (into `certs/`):

```bash
./generate-certs.sh
```

The imposter in `imposters-https.json` only says `"protocol": "https"` — it carries no `cert`/`key`.
Its certificate comes from the server-wide default, set in `docker-compose-https.yml` with
`--default-tls-cert /certs/server.crt --default-tls-key /certs/server.key`. The certificate is valid
for `localhost`, `127.0.0.1`, `rift` and `rift-https-demo`, so clients have to use one of those
names. See [TLS/HTTPS](../features/tls.md) for the other ways to give an imposter a
certificate, and for mutual TLS.

### Start

```bash
docker compose -f docker-compose-https.yml up -d
```

### Test

```bash
# Basic HTTPS request (with CA certificate)
curl --cacert certs/ca.crt https://localhost:4545/api/test
# {"message": "Secure response over HTTPS", "tls": true}

# Without the CA, verification fails — the demo CA is not in your trust store
curl https://localhost:4545/api/test
# curl: (60) SSL certificate problem: ...

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
# Using local binary. --allowInjection is required: this demo runs inject/_rift.script, and the
# configfile door enforces the same injection gate as the admin API (issue #612).
./target/release/rift-http-proxy --configfile docs/demo/imposters-scripting-engines.json --allowInjection

# Or using Docker
docker run -p 2525:2525 -p 4560:4560 \
  -v $(pwd)/docs/demo/imposters-scripting-engines.json:/imposters.json:ro \
  zainalpour/rift-proxy:latest --configfile /imposters.json --allowInjection
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
| Rhai | `_rift.script` (`ctx`) | `ctx.state.get(key)` | `ctx.request.method`, `ctx.request.path` |
| JavaScript | `_rift.script` (`ctx`), or `inject` (Mountebank) | `ctx.state.get(key)` | `ctx.request.method` |

---

## Demo 6: HTTPS Intercept Proxy (standalone)

A system under test in **its own container** sends its HTTPS traffic through rift's intercept
listener, configured only by `HTTPS_PROXY`. Rift terminates TLS with a CA the SUT already trusts,
and a rule forwards `cdn.optimizely.com` to an imposter serving an Optimizely-style datafile from
disk. The config file declares the imposter, the listener and the rule; nothing calls the admin API.
This is the shape of an ECS/Fargate task or a compose stack, where the SUT cannot be handed a CA
after it starts.

### Prerequisites

The CA has to exist before either container starts. Rift makes it offline:

```bash
cd docs/demo
./generate-intercept-ca.sh     # docker run … rift intercept-ca generate --out-dir intercept-ca
```

`rift intercept-ca` first shipped in v0.20.0. With an image older than that, pull a current one or
build and retag the image first (see the note at the top).

### Start

```bash
docker compose -f docker-compose-intercept.yml up -d --wait
```

`--wait` returns once the SUT container is healthy, and its healthcheck *is* the interception: a
`curl https://cdn.optimizely.com/datafiles/demo.json` through the proxy that must return the
datafile. CI boots this demo on every change to `docs/demo/`, so a broken intercept path fails the
build.

### Test

```bash
./test-intercept.sh
```

It fetches the datafile from the SUT container, checks rift serves the CA the SUT trusts, edits
`fixtures/datafile.json` and runs `POST /admin/reload` (the SUT then sees the new revision — no
rule or listener change), and adds a rule at runtime over the admin API. The intercept port (8080)
is not published to the host on purpose; the SUT reaches it over the compose network.

### Cleanup

```bash
docker compose -f docker-compose-intercept.yml down
```

---

## Demo 7: Retry Proxy Simulation

A Rhai script fails the first two requests of a flow with `503` and answers `200` from the third
on, the way a service that recovers after a hiccup would. Use it to test a client's retry logic.
The flow is identified by the `X-Flow-Id` header, so each flow counts its own attempts.

### Start

```bash
docker compose -f docker-compose-retry-proxy.yml up -d
```

### Test

```bash
./test-retry-proxy.sh
```

The script resets the counter for a new flow ID, sends four requests to port 4560, and prints each
status. Attempts 1 and 2 return `503` with a `Retry-After` header. Attempts 3 and 4 return `200`
with a success body. The script needs `curl`; it uses `jq` for the reset reply when `jq` is
installed.

### Cleanup

```bash
docker compose -f docker-compose-retry-proxy.yml down
```

---

## Configuration Files

| File | Description |
|:-----|:------------|
| `imposters.json` | Mountebank HTTP imposter config |
| `imposters-rift-features.json` | Fault injection demo config |
| `imposters-scripting.json` | Scripting with flow state demo config |
| `imposters-scripting-engines.json` | Multi-engine scripting demo (Rhai, JS) |
| `imposters-https.json` | HTTPS/TLS demo config |
| `imposters-retry-proxy.json` | Retry proxy demo config |
| `docker-compose.yml` | HTTP demo |
| `docker-compose-https.yml` | HTTPS/TLS demo |
| `docker-compose-rift-features.yml` | Fault injection demo |
| `docker-compose-scripting.yml` | Scripting with flow state demo |
| `docker-compose-scripting-engines.yml` | Multi-engine scripting demo (Docker alternative for Demo 5) |
| `docker-compose-retry-proxy.yml` | Retry proxy demo |
| `test-retry-proxy.sh` | Retry proxy demo check |
| `generate-certs.sh` | Certificate generation script |
| `imposters-intercept.json` | Intercept demo: datafile imposter plus the `intercept` block |
| `docker-compose-intercept.yml` | Intercept demo: rift plus a curl SUT container |
| `generate-intercept-ca.sh` | Makes the intercept demo's CA with `rift intercept-ca generate` |
| `test-intercept.sh` | Intercept demo end-to-end check (datafile edit + reload) |

---

## Rift Extensions (`_rift` namespace)

Rift extends Mountebank with advanced features through the `_rift` namespace:

- **Flow State**: Stateful testing with the in-memory backend
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

See the [Rift Extensions documentation](../configuration/native.md) for more details.
