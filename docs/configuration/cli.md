---
layout: default
title: CLI Reference
parent: Configuration
nav_order: 3
---

# Command Line Reference

Rift provides Mountebank-compatible CLI options for easy migration.

---

## Basic Usage

```bash
# Start the server
rift-http-proxy

# With configuration file
rift-http-proxy --configfile imposters.json

# With custom port
rift-http-proxy --port 3525
```

---

## CLI Options

```bash
rift-http-proxy [OPTIONS]

Options:
      --port <PORT>                Admin API port [default: 2525]
      --host <HOST>                Bind hostname [default: 0.0.0.0]
      --configfile <FILE>          Load imposters from a JSON/YAML file on startup
      --datadir <DIR>              Directory for persistent imposter storage
      --allow-injection            Enable JavaScript injection in responses (alias: --allowInjection)
      --local-only                 Only accept connections from localhost
      --loglevel <LEVEL>           Log level: debug, info, warn, error [default: info]
      --metrics-port <PORT>        Prometheus metrics port [default: 9090]
      --ip-whitelist <IPS>         Comma-separated allowed IPs
      --mock                       Run in mock mode
      --debug                      Enable debug mode
      --nologfile                  Disable log file (stdout only)
      --log <FILE>                 Log file path
      --pidfile <FILE>             PID file path
      --origin <ORIGIN>            CORS allowed origin
      --api-key <TOKEN>            Require this token in the Authorization header for all admin API requests
      --rcfile <FILE>              RC file of default flag values (a subset: port/host/loglevel/allowInjection/localOnly/datadir/configfile)
      --default-tls-cert <FILE>    Default TLS certificate (PEM) for HTTPS imposters without their own
      --default-tls-key <FILE>     Default TLS private key (PEM), paired with --default-tls-cert
      --no-self-signed-tls         Disable the self-signed fallback; an HTTPS imposter with no cert is an error
      --no-parse                   Disable EJS preprocessing of --configfile (alias: --noParse)
      --formatter <NAME>           Custom config formatter module (no-op; Rift auto-detects JSON/YAML)
      --protofile <FILE>           Custom protocol definitions file (no-op; custom protocols unsupported)
  -h, --help                       Print help
  -V, --version                    Print version
```

`--no-parse` disables EJS preprocessing of `--configfile` (`<% include %>` / `<%= process.env.X %>`
expansion), which is otherwise applied on load. `--formatter` and `--protofile` are accepted for
Mountebank command-line compatibility but have no effect in Rift.

### API-key authentication

`--api-key` (or `MB_APIKEY`) requires every admin API request to carry the token in the
`Authorization` header. Data-plane traffic — direct imposter ports and the `/__rift/:port/...`
gateway — is **not** gated by this key.

```bash
rift-http-proxy --api-key s3cr3t
curl -H "Authorization: s3cr3t" http://localhost:2525/imposters
```

### Default TLS for HTTPS imposters

An imposter declared with `protocol: https` terminates TLS. If it carries no `cert`/`key`, Rift
falls back to `--default-tls-cert` / `--default-tls-key` when set, otherwise to a generated
self-signed certificate. Pass `--no-self-signed-tls` to turn a missing certificate into a startup
error instead of silently self-signing.

```bash
rift-http-proxy \
  --default-tls-cert ./certs/server.pem \
  --default-tls-key ./certs/server-key.pem \
  --no-self-signed-tls
```

### Examples

```bash
# Start with custom port
rift-http-proxy --port 3525

# Load configuration and enable injection
rift-http-proxy --configfile imposters.json --allow-injection

# Debug logging
rift-http-proxy --loglevel debug

# Restrict access
rift-http-proxy --local-only
rift-http-proxy --ip-whitelist "192.168.1.0/24,10.0.0.0/8"

# With persistent data directory
rift-http-proxy --datadir ./mb-data
```

---

## Environment Variables

Environment variables override CLI defaults:

| Variable | Description | Default |
|:---------|:------------|:--------|
| `MB_PORT` | Admin API port | `2525` |
| `MB_HOST` | Bind hostname | `0.0.0.0` |
| `MB_CONFIGFILE` | Imposter config file | |
| `MB_DATADIR` | Persistent storage directory | |
| `MB_ALLOW_INJECTION` | Enable injection (`true`/`false`) | `false` |
| `MB_LOCAL_ONLY` | Localhost only | `false` |
| `MB_LOGLEVEL` | Log level | `info` |
| `MB_APIKEY` | Admin API authorization token (see `--api-key`) | |
| `RIFT_METRICS_PORT` | Prometheus metrics port | `9090` |
| `RIFT_DEFAULT_TLS_CERT` | Default TLS certificate (PEM) for HTTPS imposters | |
| `RIFT_DEFAULT_TLS_KEY` | Default TLS private key (PEM) | |
| `RIFT_NO_SELF_SIGNED_TLS` | Disable self-signed TLS fallback (`true`/`false`) | `false` |
| `RUST_LOG` | Detailed log configuration | `info` |

### Docker Example

```bash
docker run \
  -e MB_PORT=2525 \
  -e MB_ALLOW_INJECTION=true \
  -e RUST_LOG=debug \
  -p 2525:2525 \
  -p 9090:9090 \
  zainalpour/rift-proxy:latest
```

### Docker Compose Example

```yaml
version: '3.8'
services:
  rift:
    image: zainalpour/rift-proxy:latest
    ports:
      - "2525:2525"
      - "4545:4545"
      - "9090:9090"
    environment:
      - MB_PORT=2525
      - MB_ALLOW_INJECTION=true
      - RUST_LOG=info
    volumes:
      - ./imposters.json:/imposters.json
    command: ["--configfile", "/imposters.json"]
```

---

## Logging Configuration

### Log Levels

```bash
# Via CLI
rift-http-proxy --loglevel debug

# Via environment
RUST_LOG=debug rift-http-proxy
```

| Level | Description |
|:------|:------------|
| `error` | Only errors |
| `warn` | Warnings and errors |
| `info` | Standard operation (default) |
| `debug` | Detailed debugging |
| `trace` | Very verbose (development) |

### Module-Specific Logging

```bash
# Debug only rift modules
RUST_LOG=rift=debug rift-http-proxy

# Debug HTTP handling
RUST_LOG=rift::http=debug rift-http-proxy

# Multiple modules
RUST_LOG=rift=info,rift::proxy=debug rift-http-proxy
```

---

## Health Check

Rift provides health endpoints:

```bash
# Admin API health
curl http://localhost:2525/

# Metrics health
curl http://localhost:9090/metrics
```

---

## Signal Handling

| Signal | Action |
|:-------|:-------|
| `SIGTERM` | Graceful shutdown |
| `SIGINT` | Graceful shutdown (Ctrl+C) |

```bash
# Graceful shutdown
kill -TERM $(pidof rift-http-proxy)

# Force kill (not recommended)
kill -9 $(pidof rift-http-proxy)
```

---

## Exit Codes

| Code | Meaning |
|:-----|:--------|
| `0` | Success |
| `1` | General error |
| `2` | Configuration error |
| `3` | Port binding error |

---

## Subcommands

Rift supports several subcommands for server management:

### start

Start the Rift server (default behavior when no subcommand is specified):

```bash
rift-http-proxy start
rift-http-proxy start --port 3525 --configfile imposters.json
```

### stop

Stop a running Rift server using its PID file:

```bash
# Stop server using default PID file (rift.pid)
rift-http-proxy stop

# Stop using custom PID file
rift-http-proxy stop --pidfile /var/run/rift.pid
```

### restart

Restart a running Rift server:

```bash
rift-http-proxy restart --pidfile /var/run/rift.pid
```

### save

Save current imposters to a file for later replay:

```bash
# Save imposters to file
rift-http-proxy save --savefile recorded.json

# Save with proxies removed (pure recorded responses)
rift-http-proxy save --savefile mocks.json --remove-proxies
```

### replay

Replay saved imposters from a file:

```bash
rift-http-proxy replay --configfile recorded.json
```

---

## Additional CLI Tools

Rift includes additional CLI tools for working with imposters:

### rift-verify

Test imposters by making requests and verifying responses.

```bash
rift-verify [OPTIONS]

Options:
  -a, --admin-url <URL>   Rift admin API URL [default: http://localhost:2525]
  -p, --port <PORT>       Verify specific imposter port only
  -c, --show-curl         Show curl commands for each test
  -v, --verbose           Verbose output with pass/fail details
  -t, --timeout <SECS>    Request timeout in seconds [default: 10]
      --dry-run           Show what would be tested without making requests
      --skip-dynamic      Skip stubs with inject/proxy/script responses
      --status-only       Only verify status codes (ignore body/headers)
      --demo              Run demo showing enhanced error output
  -h, --help              Print help
  -V, --version           Print version
```

**Examples:**

```bash
# Verify all imposters
rift-verify

# Verify specific imposter with curl commands
rift-verify --port 4545 --show-curl

# Dry run to see test plan
rift-verify --dry-run --verbose

# Skip dynamic stubs (proxy, inject, script)
rift-verify --skip-dynamic

# Status-only mode for cycling responses
rift-verify --status-only
```

See [Stub Analysis]({{ site.baseurl }}/features/stub-analysis/) for details.

### rift-lint

Validate imposter configuration files before loading.

```bash
rift-lint <path> [OPTIONS]

Arguments:
  <path>              Path to imposter file or directory

Options:
  -f, --fix           Fix issues automatically where possible
  -o, --output <FMT>  Output format: text (default), json
  -e, --errors-only   Only show errors (hide warnings)
  -v, --verbose       Verbose output
  -s, --strict        Strict mode - treat warnings as errors
  -h, --help          Print help
  -V, --version       Print version
```

**Examples:**

```bash
# Lint all imposters in directory
rift-lint ./imposters/

# Strict mode for CI/CD (exits 1 on warnings)
rift-lint ./imposters/ --strict

# JSON output for tooling integration
rift-lint ./imposters/ --output json

# Auto-fix header type issues
rift-lint ./imposters/ --fix

# Only show errors, hide warnings
rift-lint ./imposters/ --errors-only
```

See [Configuration Linting]({{ site.baseurl }}/features/linting/) for details.
