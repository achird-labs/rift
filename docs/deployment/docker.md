---
layout: default
title: Docker
parent: Deployment
nav_order: 1
---

# Docker Deployment

Deploy Rift using Docker for quick setup and consistent environments.

---

## Quick Start

```bash
# Pull the image (from GitHub Container Registry)
docker pull zainalpour/rift-proxy:latest

# Or from Docker Hub
docker pull zainalpour/rift-proxy:latest

# Run with default settings
docker run -p 2525:2525 zainalpour/rift-proxy:latest
```

---

## Image Flavors

Every release publishes two flavors of the same rift binary. They are interchangeable — same admin
API, same imposter behaviour, same ports, same environment variables.

| Tag | Base | Use it when |
|:----|:-----|:------------|
| `latest`, `X.Y.Z` | `debian:trixie-slim` | The default. |
| `latest-static`, `X.Y.Z-static` | `scratch` | You want the smallest CVE surface — typically ephemeral CI/test environments. |

```bash
docker pull zainalpour/rift-proxy:latest-static
docker run -p 2525:2525 zainalpour/rift-proxy:latest-static
```

The `-static` flavor is a statically-linked musl build on `FROM scratch`. It contains the rift
binary, a CA certificate bundle, and a passwd entry — nothing else. No package manager ever runs in
it, so an image scanner finds **no OS packages to report**: there is no base distro to keep patching
just because rift is running in your test suite.

Two consequences worth knowing before you switch:

- **No shell.** There is no `/bin/sh`, so `docker exec ... sh`, string-form `command:` overrides, and
  shell-form healthchecks do not work. Use exec form (`["rift", "healthcheck"]`) and pass flags
  directly. The health probe is built into the binary precisely for this reason — see
  [`rift healthcheck`]({{ site.baseurl }}/configuration/cli/).
- **No mimalloc.** The musl binaries are built without the mimalloc allocator (it is a default
  feature of the glibc builds). Scripting and the Redis backend are both present. If you are
  benchmarking allocation-heavy workloads, use the default flavor.

HTTPS upstream proxying works in both: the CA bundle is copied into the static image, because rift's
TLS client loads the OS trust store at runtime.

---

## Verifying an Image

Published images carry an SBOM and max-mode provenance, and are signed with
[cosign](https://docs.sigstore.dev/) keyless — the signing identity is the release workflow itself,
so there is no public key to distribute.

```bash
# Verify the signature and its provenance
cosign verify \
  --certificate-identity-regexp 'https://github.com/EtaCassiopeia/rift/.github/workflows/.+' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  zainalpour/rift-proxy:latest-static

# Inspect the SBOM / provenance attestations
docker buildx imagetools inspect zainalpour/rift-proxy:latest-static \
  --format '{{ json .SBOM }}'
docker buildx imagetools inspect zainalpour/rift-proxy:latest-static \
  --format '{{ json .Provenance }}'
```

---

## Basic Configuration

### With Environment Variables

```bash
docker run -d \
  --name rift \
  -p 2525:2525 \
  -p 9090:9090 \
  -e MB_PORT=2525 \
  -e MB_ALLOW_INJECTION=true \
  -e RUST_LOG=info \
  zainalpour/rift-proxy:latest
```

### With Configuration File

```bash
docker run -d \
  --name rift \
  -p 2525:2525 \
  -p 4545:4545 \
  -v $(pwd)/imposters.json:/imposters.json:ro \
  zainalpour/rift-proxy:latest \
  --configfile /imposters.json
```

---

## Docker Compose

### Basic Setup

```yaml
# docker-compose.yml
version: '3.8'

services:
  rift:
    image: zainalpour/rift-proxy:latest
    container_name: rift
    ports:
      - "2525:2525"    # Admin API
      - "4545:4545"    # Imposter port
      - "9090:9090"    # Metrics
    environment:
      - MB_PORT=2525
      - MB_ALLOW_INJECTION=true
      - RUST_LOG=info
    volumes:
      - ./imposters.json:/imposters.json:ro
    command: ["--configfile", "/imposters.json"]
    healthcheck:
      test: ["CMD", "rift", "healthcheck"]
      interval: 10s
      timeout: 5s
      retries: 3
```

### With Multiple Ports

```yaml
services:
  rift:
    image: zainalpour/rift-proxy:latest
    ports:
      - "2525:2525"    # Admin
      - "4545:4545"    # User Service
      - "4546:4546"    # Order Service
      - "4547:4547"    # Payment Service
      - "9090:9090"    # Metrics
    volumes:
      - ./imposters.json:/imposters.json:ro
    command: ["--configfile", "/imposters.json"]
```

### With TLS

```yaml
services:
  rift:
    image: zainalpour/rift-proxy:latest
    ports:
      - "2525:2525"
      - "4545:4545"
    volumes:
      - ./imposters.json:/imposters.json:ro
      - ./certs:/certs:ro
    command: ["--configfile", "/imposters.json"]
```

---

## Integration Testing Setup

### Rift with Your Application

```yaml
version: '3.8'

services:
  # Your application
  app:
    build: .
    environment:
      - USER_SERVICE_URL=http://rift:4545
      - ORDER_SERVICE_URL=http://rift:4546
    depends_on:
      rift:
        condition: service_healthy

  # Mock server
  rift:
    image: zainalpour/rift-proxy:latest
    ports:
      - "2525:2525"
    volumes:
      - ./test/mocks:/mocks:ro
    command: ["--configfile", "/mocks/imposters.json"]
    healthcheck:
      test: ["CMD", "rift", "healthcheck"]
      interval: 5s
      timeout: 3s
      retries: 10
```

### Test Runner Integration

```yaml
services:
  rift:
    image: zainalpour/rift-proxy:latest
    ports:
      - "2525:2525"
      - "4545:4545"
    healthcheck:
      test: ["CMD", "rift", "healthcheck"]
      interval: 5s
      timeout: 3s
      retries: 10

  tests:
    build:
      context: .
      dockerfile: Dockerfile.test
    environment:
      - MOCK_SERVER_URL=http://rift:4545
      - MOCK_ADMIN_URL=http://rift:2525
    depends_on:
      rift:
        condition: service_healthy
    command: ["npm", "test"]
```

---

## Production Configuration

### Resource Limits

```yaml
services:
  rift:
    image: zainalpour/rift-proxy:latest
    deploy:
      resources:
        limits:
          cpus: '2'
          memory: 512M
        reservations:
          cpus: '0.5'
          memory: 128M
```

### Logging

```yaml
services:
  rift:
    image: zainalpour/rift-proxy:latest
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"
    environment:
      - RUST_LOG=warn
```

### Restart Policy

```yaml
services:
  rift:
    image: zainalpour/rift-proxy:latest
    restart: unless-stopped
```

---

## Building Custom Image

### Dockerfile

```dockerfile
FROM zainalpour/rift-proxy:latest

# Copy configuration
COPY imposters.json /config/imposters.json

# Set environment
ENV MB_PORT=2525
ENV MB_ALLOW_INJECTION=true

# Run with config
CMD ["--configfile", "/config/imposters.json"]
```

### Build and Run

```bash
docker build -t my-rift:latest .
docker run -p 2525:2525 -p 4545:4545 my-rift:latest
```

### Feature flags for a custom build

`crates/rift-http-proxy/Dockerfile` builds with `ARG FEATURES=javascript,redis-backend` by
default. If you're building a slimmer image (or embedding Rift as a `cdylib` instead of running the
container), see the Cargo feature table in [FFI (C-ABI)]({{ site.baseurl }}/embedding/ffi/#cargo-features)
and the [Embedding & SPI]({{ site.baseurl }}/embedding/) overview — the same features gate both the
binary and the `rift-ffi` cdylib, and `cargo build --no-default-features` (plus an explicit
`--features` list) drops the scripting engines and Redis backend you don't need.

---

## Common Operations

### View Logs

```bash
docker logs rift
docker logs -f rift  # Follow
```

### Drive the Admin API

The images ship the rift binary and nothing else — no curl, and in the `-static` flavor no shell
either — so run these from the host against the published admin port rather than via `docker exec`.

```bash
# Create imposter
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{"port": 4545, "protocol": "http", "stubs": []}'

# List imposters
curl http://localhost:2525/imposters
```

The one thing worth running *inside* the container is the health probe, which is built in:

```bash
docker exec rift rift healthcheck && echo healthy
```

### Restart

```bash
docker restart rift
```

### Clean Up

```bash
docker stop rift
docker rm rift
docker compose down -v
```

---

## Troubleshooting

### Container Won't Start

```bash
# Check logs
docker logs rift

# Verify config
docker run --rm -v $(pwd)/imposters.json:/imposters.json \
  zainalpour/rift-proxy:latest --validate /imposters.json
```

### Port Already in Use

```bash
# Find process using port
lsof -i :2525

# Use different host port
docker run -p 3525:2525 zainalpour/rift-proxy:latest
```

### Permission Denied

```bash
# Fix volume permissions
chmod 644 imposters.json
```
