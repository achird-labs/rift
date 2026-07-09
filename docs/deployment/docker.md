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
      test: ["CMD", "curl", "-f", "http://localhost:2525/"]
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
      test: ["CMD", "curl", "-f", "http://localhost:2525/"]
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
      test: ["CMD", "curl", "-f", "http://localhost:2525/"]
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

### Execute Commands

```bash
# Create imposter
docker exec rift curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{"port": 4545, "protocol": "http", "stubs": []}'

# List imposters
docker exec rift curl http://localhost:2525/imposters
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
