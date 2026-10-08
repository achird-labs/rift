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
# Pull the image from Docker Hub
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
| `latest`, `vX.Y.Z` (e.g. `v0.17.0`) | `debian:bookworm-slim` | The default. |
| `latest-static`, `vX.Y.Z-static` | `scratch` | You want the smallest CVE surface — typically ephemeral CI/test environments. |

Both are multi-arch (`linux/amd64`, `linux/arm64`). Version tags keep the leading `v` of the release
tag. Both run as the unprivileged user `rift` (uid 1000) with `/data` as the working directory, and
set `MB_PORT=2525`, `MB_HOST=0.0.0.0`, `MB_LOGLEVEL=info` and `RIFT_METRICS_PORT=9090`. The
entrypoint is `rift`, so arguments after the image name are `rift` flags or a subcommand.

The images declare `EXPOSE 2525 9090 4545-4550 8080-8090` as documentation only — publish the ports
your imposters actually use with `-p`.

The linter ships as its own image, `zainalpour/rift-lint` (`latest`, `vX.Y.Z`); its entrypoint is
`rift-lint` and its working directory is `/imposters`.

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

  The images' built-in `HEALTHCHECK` runs `rift healthcheck` (every 30s, 3s timeout, 3 retries,
  5s start period), which probes `/health` on the host and port the container's own `MB_HOST` /
  `MB_PORT` say. Moving the port with `-e MB_PORT=…` is therefore followed automatically. Moving it
  with a `--port` argument or an `--rcfile` is **not** — the probe never sees the server's command
  line — so override the healthcheck to tell the probe the same thing, or it will report unhealthy
  forever (issue #1133):

  ```yaml
  healthcheck:
    test: ["CMD", "rift", "--rcfile", "/etc/rift/rc.json", "healthcheck"]
  ```
- **No mimalloc.** The musl binaries are built without the mimalloc allocator. Scripting is
  present. If you are benchmarking allocation-heavy workloads, use the
  default flavor on `amd64` (the `arm64` default image is built without mimalloc too).

HTTPS upstream proxying works in both: the CA bundle is copied into the static image, because rift's
TLS client loads the OS trust store at runtime. For a private CA, see
[Reaching an Origin Behind a Private CA]({{ site.baseurl }}/deployment/#reaching-an-origin-behind-a-private-ca).

### Health checks and `--api-key`

**Set the key with `MB_APIKEY`, and the built-in `HEALTHCHECK` needs no change** (issue #1154): the
probe is a separate process, but it runs with the container's environment, so it reads `MB_APIKEY`
and presents it to the admin API's `/health`. It sends the key only to the address derived from
`MB_HOST`/`MB_PORT`, never to an explicit `--url`.

The other two spellings do **not** reach it on their own, for the reason above — the probe never
sees the server's command line:

- **`--api-key` as a `docker run` argument** is invisible to the probe, so the container reports
  unhealthy. Switch to `-e MB_APIKEY=…`. Do not fix it by adding `--api-key` to the healthcheck
  `test:` line — that puts the key in `docker inspect`.
- **An rcfile `apiKey`** reaches the probe only if the healthcheck is given the same file, as in the
  `--rcfile` override above: `["CMD", "rift", "--rcfile", "/etc/rift/rc.json", "healthcheck"]`.

### Stopping the container

`docker stop` is prompt: `rift` handles `SIGTERM` itself, including as the container's PID 1, so no
init process is needed (issue #1155). It stops accepting, gives in-flight admin connections a
bounded grace of about three seconds at most, closes imposter connections, leaves any `--datadir`
state in place, and exits `0`. Before this, `rift` had no signal handler — the kernel discards an
unhandled `SIGTERM` to PID 1 — so `docker stop` waited out its full timeout and killed it, exit
`137`; running under an init process was the workaround and is no longer needed.

---

## Verifying an Image

Published images carry an SBOM and max-mode provenance, and are signed with
[cosign](https://docs.sigstore.dev/) keyless — the signing identity is the release workflow itself,
so there is no public key to distribute.

{% raw %}
```bash
# Verify the signature and its provenance
cosign verify \
  --certificate-identity-regexp 'https://github.com/achird-labs/rift/.github/workflows/.+' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  zainalpour/rift-proxy:latest-static

# Inspect the SBOM / provenance attestations
docker buildx imagetools inspect zainalpour/rift-proxy:latest-static \
  --format '{{ json .SBOM }}'
docker buildx imagetools inspect zainalpour/rift-proxy:latest-static \
  --format '{{ json .Provenance }}'
```
{% endraw %}

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
  -e MB_LOGLEVEL=info \
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
      - MB_LOGLEVEL=info
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

Mount the certificate and key, and name them as the default for HTTPS imposters that carry no
`cert`/`key` of their own:

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
    command:
      - "--configfile"
      - "/imposters.json"
      - "--default-tls-cert"
      - "/certs/server.pem"
      - "--default-tls-key"
      - "/certs/server-key.pem"
```

See [TLS/HTTPS]({{ site.baseurl }}/features/tls/) for per-imposter certificates, mutual TLS and the
self-signed fallback.

### Intercepting HTTPS from another container

To mock an external HTTPS host your application hard-codes, run the
[intercept listener]({{ site.baseurl }}/features/intercept-proxy/) beside it and send the app's
HTTPS through it. Declare the listener and its rules in the config file, so it is correct the moment
`rift` is healthy:

```json
{
  "imposters": [
    { "port": 4545, "protocol": "http",
      "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "{\"featureX\":\"ON\"}" } }] }] }
  ],
  "intercept": {
    "host": "0.0.0.0",
    "port": 8080,
    "caCertPath": "/ca/ca-cert.pem",
    "caKeyPath": "/ca/ca-key.pem",
    "rules": [
      { "host": "cdn.example.com", "action": { "forward": { "port": 4545 } } }
    ]
  }
}
```

The CA for a config-file listener comes from the block: `caCertPath` / `caKeyPath` (files, as here),
`caCertPem` / `caKeyPem` (inline), or `caCertPemEnv` / `caKeyPemEnv` (the names of two environment
variables holding the PEMs — e.g. set by CI or an orchestrator that injects secrets as variables;
Docker/Compose secrets are mounted as files under `/run/secrets`, so use the path pair for those). The `RIFT_INTERCEPT_*` variables and `--intercept-*` flags
belong to the other way of starting a listener -- `--intercept-port` with no block, rules added over
the admin API afterwards. Setting any of them **alongside a block is a startup error**, so do not put
`RIFT_INTERCEPT_CA_CERT_PEM` (or any `RIFT_INTERCEPT_*`) in the environment of a container that loads
a block. Make the CA once with `rift intercept-ca generate --out-dir ./ca` (see
[`intercept-ca`]({{ site.baseurl }}/configuration/cli/#intercept-ca)) and mount it:

```yaml
services:
  rift:
    image: zainalpour/rift-proxy:latest
    volumes:
      - ./mock.json:/config/mock.json:ro
      - ./ca:/ca:ro                      # ca-cert.pem + ca-key.pem from `intercept-ca generate`
    command: ["--configfile", "/config/mock.json"]
    # No `ports:` for 8080 -- only the app, on the compose network, needs the listener.
    healthcheck:
      test: ["CMD", "rift", "healthcheck"]
      interval: 5s
      timeout: 3s
      retries: 10

  app:
    build: .
    depends_on:
      rift:
        condition: service_healthy
    environment:
      HTTPS_PROXY: http://rift:8080
      NO_PROXY: localhost,127.0.0.1
      SSL_CERT_FILE: /certs/ca.pem          # curl, Go, OpenSSL clients
      REQUESTS_CA_BUNDLE: /certs/ca.pem     # Python requests
      NODE_EXTRA_CA_CERTS: /certs/ca.pem    # Node
    volumes:
      - ./ca/ca-cert.pem:/certs/ca.pem:ro
```

A JVM app reads its truststore once at startup, so give it a JKS built ahead of time:
`rift intercept-ca export --cert ./ca/ca-cert.pem --format jks --out truststore.jks --merge-system-cas /etc/ssl/certs/ca-certificates.crt`
(password from `RIFT_TRUSTSTORE_PASSWORD`), then `-Djavax.net.ssl.trustStore=…`. Which variable adds
to the default roots and which replaces them is spelled out under
[Trusting the CA from the SUT]({{ site.baseurl }}/features/intercept-proxy/#trusting-the-ca-from-the-sut);
it is not repeated here.

Three things that fail quietly in containers:

- **The bind host.** `POST /intercept` defaults to `127.0.0.1`, which another container cannot
  reach. Name `"host": "0.0.0.0"` in the config block (as above); a bare `--intercept-port` binds
  on `--host`, default `0.0.0.0`, and warns unless `--intercept-auth` is set. Keep the listener off
  published ports unless something outside the network needs it.
- **Key file permissions.** The image runs as uid 1000, and `intercept-ca generate` writes the key
  `0600` as you, so on a Linux bind mount rift cannot read it and fails at startup. For a CI or demo
  CA only, `chmod 644 ca/ca-key.pem` (or run the container with a `user:` / group that matches the
  file's owner); never loosen a real key. See the note in
  [`generate-certs.sh`](https://github.com/achird-labs/rift/blob/master/docs/demo/generate-certs.sh)
  for the trap.
- **`forward` reaches the rift container.** A rule's `forward` defaults to `127.0.0.1:{port}`
  inside the rift container. To reach an imposter elsewhere, name it:
  `{"forward": {"host": "mock-svc", "port": 4600, "scheme": "https"}}` -- see
  [Forward to one of your imposters]({{ site.baseurl }}/features/intercept-proxy/#forward-to-one-of-your-imposters).

Changing a rule needs only an edited file and `POST /admin/reload`; the listener itself (host, port,
CA) is fixed at boot. The CI-verified, runnable version of this setup is the
[HTTPS intercept demo](https://github.com/achird-labs/rift/tree/master/docs/demo#demo-6-https-intercept-proxy-standalone).

---

## Integration Testing Setup

### Rift with Your Application

```yaml
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
      - MB_LOGLEVEL=warn
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

The published images are assembled from the release binaries. To build an image from source
instead, use `crates/rift-http-proxy/Dockerfile` from the repository root
(`docker build -f crates/rift-http-proxy/Dockerfile .`); its runtime stage is `debian:trixie-slim`,
and it builds with `ARG FEATURES=javascript` by default (on top of the crate's default
features). If you're building a slimmer image (or embedding Rift as a `cdylib` instead of running the
container), see the Cargo feature table in [FFI (C-ABI)]({{ site.baseurl }}/embedding/ffi/#cargo-features)
and the [Embedding & SPI]({{ site.baseurl }}/embedding/) overview — the same features gate both the
binary and the `rift-ffi` cdylib, and `cargo build --no-default-features` (plus an explicit
`--features` list) drops the scripting engines you don't need.

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

# Verify config (rift-lint ships as its own image; the server image does not include it)
docker run --rm -v $(pwd):/imposters zainalpour/rift-lint imposters.json
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
