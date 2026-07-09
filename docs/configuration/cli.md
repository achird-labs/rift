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
      --scripts-dir <DIR>          Root directory for admin-API `file:`/`ref:` script resolution; references that escape it are rejected (unset ⇒ file-backed scripts via the admin API are refused)
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
      --intercept-port <PORT>      Start the TLS-MITM intercept/redirect proxy on this port (epic #394); off when unset
      --intercept-ca-cert <FILE>   PEM CA certificate for interception (with --intercept-ca-key); a CA is generated if omitted
      --intercept-ca-key <FILE>    PEM CA private key for interception (required with --intercept-ca-cert)
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
| `RIFT_SCRIPTS_DIR` | Root directory for admin-API `file:`/`ref:` script resolution (env alias of `--scripts-dir`); references escaping it are rejected | |
| `RIFT_DEBUG` | Enable debug mode (truthy: `1`/`true`/`yes`/`on`); same as `--debug`. Adds an `x-rift-script-trace` response header and makes response-template errors return a request-time error instead of an empty substitution | off |
| `RIFT_METRICS_PORT` | Prometheus metrics port | `9090` |
| `RIFT_DEFAULT_TLS_CERT` | Default TLS certificate (PEM) for HTTPS imposters | |
| `RIFT_DEFAULT_TLS_KEY` | Default TLS private key (PEM) | |
| `RIFT_NO_SELF_SIGNED_TLS` | Disable self-signed TLS fallback (`true`/`false`) | `false` |
| `RIFT_INTERCEPT_PORT` | Start the intercept/TLS-MITM proxy on this port (epic #394) | |
| `RIFT_INTERCEPT_CA_CERT` | PEM CA certificate for interception (with `RIFT_INTERCEPT_CA_KEY`) | |
| `RIFT_INTERCEPT_CA_KEY` | PEM CA private key for interception | |
| `RIFT_DISABLE_HTTP2` | Force HTTP/1-only listeners, disabling HTTP/2 & h2c auto-negotiation (truthy: `1`/`true`/`yes`/`on`) | off |
| `RIFT_TCP_BACKLOG` | Listen backlog for the accept loop (positive integer) | `1024` |
| `RIFT_TCP_NODELAY` | `TCP_NODELAY` on accepted sockets; set `false`/`0`/`off` to disable | on |
| `RIFT_STRICT_BEHAVIORS` | Force strict mode process-wide (truthy: `1`/`true`/`yes`/`on`): a `decorate`/`shellTransform`/binary-base64-decode failure returns `500` instead of the lenient fallback body | off |
| `NO_COLOR` | Suppress ANSI color and the decorative banner in `rift-verify` / `rift-lint` output | |
| `RUST_LOG` | Detailed log configuration | `info` |

`RIFT_DISABLE_HTTP2` is an escape hatch for clients or intermediaries that mishandle HTTP/2; see
[HTTP/2 and h2c]({{ site.baseurl }}/mountebank/imposters/#http2-and-h2c). `RIFT_TCP_BACKLOG` and
`RIFT_TCP_NODELAY` are socket-tuning knobs covered under
[Performance → Runtime socket tuning]({{ site.baseurl }}/performance/#runtime-socket-tuning).

`RIFT_STRICT_BEHAVIORS` and the per-imposter `strictBehaviors` field combine with **OR** — either
being set enables strict mode. It is orthogonal to `RIFT_STRICT_FLOW_STORE`
([Scripting]({{ site.baseurl }}/features/scripting/)): the two env vars gate unrelated failure
paths (response behaviors vs. flow-store script errors), and neither implies the other. See
[Rift Extensions → Strict Behaviors]({{ site.baseurl }}/configuration/native/#strict-behaviors-strictbehaviors)
for the full semantics.

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

### script

Validate and run `_rift.script` scripts outside a running server (no admin API, no imposter) — the
authoring loop from [Scripting]({{ site.baseurl }}/features/scripting/). Two actions:

**`rift script check <target>`** — statically validate a raw script file (`.rhai`/`.js`) or a
config file (JSON/YAML) with `_rift.script` entries: engine syntax, entrypoint presence/arity for
the intended hook, v1-shape deprecation, and (for a config) `state`-used-without-`flowState`. Exits
non-zero on any error — so a script whose entrypoint is misnamed fails here instead of at request
time.

```bash
rift-http-proxy script check scripts/fail-twice.rhai
rift-http-proxy script check scripts/decorate.js --hook respond
rift-http-proxy script check imposters.yaml            # every _rift.script in the config
```

| Flag | Description | Default |
|:-----|:------------|:--------|
| `--hook <HOOK>` | Entrypoint to check a raw script against: `respond`/`matches`/`transform`/`delay` (ignored for a config target, which is always `respond`) | `respond` |

**`rift script run <target>`** — execute a script against a fixture request and seeded flow state,
printing the decision, the mutated flow state, captured `ctx.logger` output, and the execution
duration. No server runs.

```bash
rift-http-proxy script run scripts/fail-twice.rhai --state attempts=2
rift-http-proxy script run scripts/echo.js --request fixtures/get-resource.json --flow-id t1
```

| Flag | Description | Default |
|:-----|:------------|:--------|
| `--request <FILE>` | JSON file with the request-object shape scripts see (`{method, path, headers, query, pathParams, body}`; all fields optional) | empty `GET /` |
| `--state <KEY=VALUE>` | Seed flow state before running (repeatable); the value is parsed as JSON when it parses, else stored as a string | |
| `--flow-id <ID>` | Flow id the seeded state and the script's `ctx.state`/`ctx.store` calls use | `cli` |
| `--engine <ENGINE>` | Script engine (`rhai`/`js`); inferred from the file extension when omitted | (from extension) |
| `--hook <HOOK>` | Entrypoint to run; only `respond` is wired for both engines today | `respond` |

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
  -o, --output <FMT>      Output format: text (default), json
      --dry-run           Show what would be tested without making requests
      --skip-dynamic      Skip stubs with inject/proxy/script responses
      --verify-dynamic    Opt-in: assert dynamic stubs instead of skipping them
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

# Assert dynamic stubs instead of skipping them
rift-verify --verify-dynamic

# Status-only mode for cycling responses
rift-verify --status-only

# Machine-readable summary for CI (JSON on stdout, progress on stderr)
rift-verify -o json
```

With `-o json`, `rift-verify` writes a single summary object to stdout —
`{ "imposters", "stubs", "tests", "passed", "failed", "skipped" }` — and routes all progress and
banner output to stderr, so it pipes cleanly into other tools. Color and the decorative banner are
also suppressed automatically when stdout is not a TTY (piped) or when `NO_COLOR` is set.

By default, `rift-verify` SKIPs stubs whose response is dynamic (proxy/inject/script/cycling/faults)
because their output isn't a static function of the stub — `--skip-dynamic` makes that skip explicit.
`--verify-dynamic` is the opt-in complement: it asserts those stubs instead of skipping them, using
three mechanisms — an embedded mock upstream for `proxy` stubs (verifying the proxied response and,
when `predicateGenerators` is set, the recorded-stub prepend); a `_verify` expectation sequence
(see below) run against a freshly recreated imposter for inject/script/decorate/cycling/stateful
stubs; and deterministic (`probability: 1.0` or unset) `_rift.fault` assertions for latency/error/tcp
faults. Each check runs against a throwaway imposter that is torn down afterward, so it never mutates
the imposters under test. A dynamic stub with none of these assertable markers is still surfaced as a
visible `SKIP` in the output rather than silently ignored.

See [Stub Analysis]({{ site.baseurl }}/features/stub-analysis/) for details, including the `_verify`
annotation schema.

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
