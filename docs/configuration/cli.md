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

## Imposter Sources

`--imposters` loads imposters from one or more URIs instead of a single local path. Sources are
merged in the order given, and `POST /admin/reload` re-fetches all of them.

```bash
# A local file (these three are identical)
rift-http-proxy --configfile mocks.json
rift-http-proxy --imposters file:mocks.json
rift-http-proxy --imposters mocks.json

# A document served over HTTP
rift-http-proxy --imposters https://config.example.com/imposters.json

# Several sources merged into one running set
rift-http-proxy --imposters file:base.json,https://config.example.com/team-overrides.json
```

`--configfile <p>` is sugar for `--imposters file:<p>` and behaves identically; passing both is an
error. `RIFT_IMPOSTERS` is the environment-variable spelling.

### Built-in schemes

| Scheme | Form | Version token |
|---|---|---|
| `file:` | `file:<path>`, or a bare path | none — always re-read |
| `http:` / `https:` | a full URL | the response `ETag` |

### Merging

Every source contributes its imposters to one set. A port declared by **two** sources is a
startup error naming both — the alternative, letting the last source win, silently drops an
imposter the operator asked for. The optional `intercept` and `routes` blocks follow the same
rule: at most one source may declare each.

### Reload and `ETag`

`POST /admin/reload` re-fetches every source. An `https:` source sends `If-None-Match` with the
`ETag` it last saw; a `304 Not Modified` is served from cache without re-parsing, and when *every*
source reports no change the reload returns without touching the running imposters at all:

```json
{"message": "No source changed; imposters left as they are",
 "created": 0, "replaced": 0, "stubPatched": 0, "deleted": 0}
```

When something did change, the existing incremental apply runs: unchanged imposters keep their
recorded requests, scenario state and response cyclers; only changed ports are patched or replaced.

### What a remote document may not do

A document fetched over the network is not written by someone who already has access to the
machine running Rift, so two things a local `--configfile` may do are refused for `https:`
sources:

- **`<% include 'path' %>` and `<%- stringify('path') %>`** — both read a local file. Refused with
  an error naming the tag.
- **`_rift.script` `file:` references** — refused, as they are for admin-API-created imposters
  without `--scripts-dir`.

`<%= process.env.VAR %>` **is** substituted for remote documents: environment is deployment
configuration the operator supplied to their own process. Note the consequence — a remote source
you do not control can read your process environment into an imposter response body. Point
`--imposters` only at hosts you trust as much as the config file they replace.

### Limits

| Limit | Value |
|---|---|
| Response body | 10 MB (enforced while reading, not from `Content-Length`) |
| Request timeout | 30 s |
| Redirects | followed only while the target stays `http`/`https`; at most 10 hops |

### Adding a scheme

Embedders register their own schemes on the builder:

```rust
ServerBuilder::from_cli(cli)
    .imposter_source(Arc::new(MyGitSource::new()))
    .run()
    .await
```

A source declares the schemes it claims via `ImposterSource::schemes`. Claiming a scheme that is
already registered — including a built-in — is a startup error rather than a silent override.
Providers return bytes; parsing goes through the same loader the built-ins use, so no scheme can
grow its own dialect of the config format.

---

## CLI Options

```bash
rift-http-proxy [OPTIONS]

Options:
      --port <PORT>                Admin API port [default: 2525]
      --host <HOST>                Bind hostname [default: 0.0.0.0]
      --configfile <FILE>          Load imposters from a JSON/YAML file on startup (sugar for --imposters file:<FILE>)
      --imposters <URI[,URI...]>   Load imposters from one or more source URIs: file:<path>, a bare path, or https://… (see Imposter Sources below)
      --datadir <DIR>              Directory for persistent imposter storage
      --scripts-dir <DIR>          Root directory for admin-API `file:`/`ref:` script resolution; references that escape it are rejected (unset ⇒ file-backed scripts via the admin API are refused)
      --allow-injection            Enable JavaScript injection in responses (alias: --allowInjection)
      --local-only                 Only accept connections from localhost (binds both the admin API and /metrics to loopback)
      --require-admin-auth         Refuse to start when the admin API would bind a non-loopback address with no --api-key (default: warn)
      --loglevel <LEVEL>           Log level: debug, info, warn, error [default: info]
      --runtime <MODE>             Runtime topology: work-stealing (default) or per-core[=N] (RFC-712; experimental, Linux-first — macOS falls back with a warning, Windows rejects it)
      --runtime-affinity           Pin per-core worker threads to CPU cores (with --runtime per-core; effective on Linux)
      --metrics-port <PORT>        Prometheus metrics port [default: 9090]
      --front-door <ADDR>          Serve every imposter from one address, routed by host/path/header (see Features -> Front Door)
      --ip-whitelist <IPS>         Comma-separated allowed IPs (accepted for Mountebank compatibility; NOT enforced)
      --mock                       Run in mock mode
      --debug                      Enable debug mode
      --nologfile                  Disable log file (stdout only)
      --log <FILE>                 Log file path
      --pidfile <FILE>             PID file path
      --origin <ORIGIN>            CORS allowed origin
      --api-key <TOKEN>            Require this token in the Authorization header for all admin API requests
      --rcfile <FILE>              RC file of default flag values (a subset: port/host/loglevel/allowInjection/localOnly/requireAdminAuth/datadir/configfile)
      --default-tls-cert <FILE>    Default TLS certificate (PEM) for HTTPS imposters without their own
      --default-tls-key <FILE>     Default TLS private key (PEM), paired with --default-tls-cert
      --no-self-signed-tls         Disable the self-signed fallback; an HTTPS imposter with no cert is an error
      --intercept-port <PORT>      Start the TLS-MITM intercept/redirect proxy on this port (epic #394); off when unset
      --intercept-auth <USER:PASS>  Require Proxy-Authorization: Basic on every CONNECT to the intercept proxy; open when unset
      --intercept-ca-cert <FILE>   PEM CA certificate for interception (with --intercept-ca-key); a CA is generated if omitted
      --intercept-ca-key <FILE>    PEM CA private key for interception (required with --intercept-ca-cert)
      --intercept-ca-cert-pem <PEM>  Inline PEM CA certificate for interception (with --intercept-ca-key-pem); mutually exclusive with file paths
      --intercept-ca-key-pem <PEM>   Inline PEM CA private key for interception (required with --intercept-ca-cert-pem)
      --no-parse                   Disable EJS preprocessing of --configfile/file: sources (alias: --noParse)
      --formatter <NAME>           Custom config formatter module (no-op; Rift auto-detects JSON/YAML)
      --protofile <FILE>           Custom protocol definitions file (no-op; custom protocols unsupported)
  -h, --help                       Print help
  -V, --version                    Print version
```

`--no-parse` disables EJS preprocessing of `--configfile` (`<% include %>` / `<%= process.env.X %>`
expansion), which is otherwise applied on load. `--formatter`, `--protofile` and `--ip-whitelist`
are accepted for Mountebank command-line compatibility but have no effect in Rift.

### `--ip-whitelist` does not filter anything

`--ip-whitelist` has **never** applied IP filtering in any release of Rift. It parses, and is
otherwise ignored; passing it now logs a warning saying so. Earlier versions of this page advertised
it as a way to "restrict access", including a CIDR example — that syntax was never implemented
either. If you were relying on it, you had no filtering.

This is deliberate rather than a gap waiting to be filled. Network-level access control belongs to
the network, which sees the real peer: behind a proxy, load balancer or container NAT this process
sees the hop, not the client, so an ACL enforced here would silently admit everyone unless it
trusted `X-Forwarded-For` — and trusting a client-settable header for an ACL is a vulnerability, not
a feature. Use a NetworkPolicy, security group or firewall.

What does work inside Rift:

| Goal | Use |
|:-----|:----|
| Refuse connections from other hosts | `--local-only` (binds loopback; covers `/metrics` too since 0.17.0) |
| Require a credential on the admin API | `--api-key <token>` |
| Fail startup if the admin plane is exposed and keyless | `--require-admin-auth` |

`GET /config` reports `"ipWhitelist": ["*"]`, which is accurate: every address may connect.

`--intercept-port` eagerly starts the [intercept/TLS-MITM proxy]({{ site.baseurl }}/features/intercept-proxy/)
at boot. It is no longer the only way to enable it: a server started without the flag still exposes
the runtime lifecycle endpoints (`POST`/`GET`/`DELETE /intercept`), so intercept can be turned on at
runtime over the admin API. The flag and the endpoints drive the same single listener.

A third way, and the only declarative one, is an
[`intercept` block in `--configfile`]({{ site.baseurl }}/features/intercept-proxy/#declare-it-in-the-config-file):
it starts the listener **with its rules already installed**, so a container needs no post-boot admin
call. The block and these `--intercept-*` flags are two spellings of one listener — supplying both
is a startup error rather than a silent precedence guess, so pick one. (Each flag also has a
`RIFT_INTERCEPT_*` environment variable, which counts as supplying it.)

#### The intercept proxy is unauthenticated unless you say otherwise

The listener has **no credential by default** — it is off unless asked for, and most uses are a
loopback test rig where auth is pure friction. But it is a TLS-MITM proxy: anyone who can reach the
port can route traffic through it and be served certificates forged by Rift's CA, which are trusted
wherever that CA is installed — and installing it is the whole point of the feature. On a shared or
LAN-reachable host, set a credential:

```bash
rift-http-proxy --intercept-port 8888 --intercept-auth ci:s3cr3t
```

Every `CONNECT` must then carry `Proxy-Authorization: Basic <base64(user:pass)>`; anything else gets
`407 Proxy Authentication Required`. Standard clients do this for you —
`HTTPS_PROXY=http://ci:s3cr3t@host:8888`, `curl -x http://ci:s3cr3t@host:8888`, or a JVM
`Authenticator`. A value with no `:`, or with a blank half, is a startup error rather than a
silently-disabled gate — as is `--intercept-auth` without `--intercept-port`, which would otherwise
read as protection while guarding nothing.

**On a shared host prefer `RIFT_INTERCEPT_AUTH`**: a value passed on the command line is visible to
anyone who can run `ps`.

Note this is **`Proxy-Authorization`, not the admin `--api-key`**. The two are different credentials
on purpose: `Proxy-Authorization` is hop-by-hop and is consumed here, whereas `Authorization` is
end-to-end and would be forwarded to every intercepted origin — sending your admin key onward to the
very servers you are intercepting.

`--require-admin-auth` covers this listener too: a non-loopback intercept bind with no credential
warns by default and refuses to start under that flag.

### API-key authentication

`--api-key` (or `MB_APIKEY`) requires every admin API request to carry the token in the
`Authorization` header. Data-plane traffic — direct imposter ports and the `/__rift/:port/...`
gateway — is **not** gated by this key.

A **blank** value is refused at startup rather than accepted as a key:

```
the admin API key (`--api-key` / `MB_APIKEY` / `apiKey`) is set to a blank value. …
```

`--api-key ""` (or `MB_APIKEY=` set-but-empty) would otherwise enable the auth gate and then match
every unauthenticated request, leaving the admin API open while reporting as protected. Whitespace
counts as blank. Omit the flag entirely to run the admin API explicitly unauthenticated; a key that
merely *contains* spaces is still a valid key and is compared exactly as given.

```bash
rift-http-proxy --api-key s3cr3t
curl -H "Authorization: s3cr3t" http://localhost:2525/imposters
```

### Unauthenticated admin plane on a public interface

`--host` defaults to `0.0.0.0`, so a bare `rift-http-proxy` with no `--api-key` already serves the
full admin API — which can create imposters and drive the TLS intercept proxy — on every interface
with no authentication. Since 0.17.0 that posture is stated at startup instead of being silent:

```
WARN the admin API is bound to 0.0.0.0:2525, which is reachable from outside this host, with no
     API key — anyone who can reach that address can create imposters and drive the TLS intercept
     proxy. Set `--api-key <token>` (`MB_APIKEY`), or restrict the bind with `--local-only` or
     `--host 127.0.0.1`. Set `--require-admin-auth` (`RIFT_REQUIRE_ADMIN_AUTH`) to make this a
     startup failure instead of a warning.
```

The default is a **warning, not a refusal**: containers require `0.0.0.0` (binding loopback inside
Docker makes the published port unreachable), so refusing would break the no-argument invocation
and every keyless quickstart. Fleets that want fail-closed opt in:

```bash
# Refuse to start unless the admin plane is authenticated or loopback-only
rift-http-proxy --require-admin-auth              # errors: 0.0.0.0 with no key
rift-http-proxy --require-admin-auth --api-key s3cr3t   # ok — authenticated
rift-http-proxy --require-admin-auth --local-only       # ok — not reachable off-host
```

`--require-admin-auth` gates on *authentication*, not on the address: a real `--api-key` satisfies
it on any bind. Loopback (`127.0.0.0/8`, `::1`) satisfies it with no key. `0.0.0.0` and `::` are
unspecified addresses, not loopback, so they are flagged — as is any specific off-host interface
such as `10.0.0.5`.

The same rule applies at every door onto the admin plane, so an embedded host gets it too: the
C-ABI `rift_serve_admin` accepts `"requireAdminAuth": true` in its options, and an embedder building
an `AdminApiServer` directly gets it from `.with_require_admin_auth(true)`.

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
rift-http-proxy --api-key s3cr3t --require-admin-auth

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
| `RIFT_REQUIRE_ADMIN_AUTH` | Refuse to start on a keyless non-loopback admin bind (env alias of `--require-admin-auth`) | `false` |
| `MB_LOGLEVEL` | Log level | `info` |
| `MB_APIKEY` | Admin API authorization token (see `--api-key`) | |
| `RIFT_SCRIPTS_DIR` | Root directory for admin-API `file:`/`ref:` script resolution (env alias of `--scripts-dir`); references escaping it are rejected | |
| `RIFT_DEBUG` | Enable debug mode (truthy: `1`/`true`/`yes`/`on`); same as `--debug`. Adds an `x-rift-script-trace` response header and makes response-template errors return a request-time error instead of an empty substitution | off |
| `RIFT_RUNTIME` | Runtime topology (env alias of `--runtime`): `work-stealing` or `per-core[=N]` (RFC-712; experimental) | `work-stealing` |
| `RIFT_RUNTIME_AFFINITY` | Pin per-core worker threads to CPU cores (env alias of `--runtime-affinity`) | off |
| `RIFT_METRICS_PORT` | Prometheus metrics port | `9090` |
| `RIFT_FRONT_DOOR` | Front-door bind address (env alias of `--front-door`): `HOST:PORT` or a bare port | off |
| `RIFT_DEFAULT_TLS_CERT` | Default TLS certificate (PEM) for HTTPS imposters | |
| `RIFT_DEFAULT_TLS_KEY` | Default TLS private key (PEM) | |
| `RIFT_NO_SELF_SIGNED_TLS` | Disable self-signed TLS fallback (`true`/`false`) | `false` |
| `RIFT_INTERCEPT_PORT` | Start the intercept/TLS-MITM proxy on this port (epic #394) | |
| `RIFT_INTERCEPT_AUTH` | `user:pass` required in `Proxy-Authorization` on every `CONNECT` to the intercept proxy (env alias of `--intercept-auth`); open when unset | |
| `RIFT_INTERCEPT_CA_CERT` | PEM CA certificate **file** for interception (with `RIFT_INTERCEPT_CA_KEY`) | |
| `RIFT_INTERCEPT_CA_KEY` | PEM CA private key **file** for interception | |
| `RIFT_INTERCEPT_CA_CERT_PEM` | Inline PEM CA certificate (the bytes, not a path; with `RIFT_INTERCEPT_CA_KEY_PEM`) — mutually exclusive with the `_CA_CERT`/`_CA_KEY` file pair | |
| `RIFT_INTERCEPT_CA_KEY_PEM` | Inline PEM CA private key for interception | |
| `RIFT_DISABLE_HTTP2` | Force HTTP/1-only listeners, disabling HTTP/2 & h2c auto-negotiation (truthy: `1`/`true`/`yes`/`on`) | off |
| `RIFT_TCP_BACKLOG` | Listen backlog for the accept loop (positive integer) | `1024` |
| `RIFT_TCP_NODELAY` | `TCP_NODELAY` on accepted sockets; set `false`/`0`/`off` to disable | on |
| `RIFT_HTTP_MAX_BUF` | Per-connection HTTP read/write buffer cap, in bytes (positive integer; floored at hyper's 8 KB minimum). Bounds per-connection memory at high connection counts | `65536` |
| `RIFT_HTTP_HEADER_TIMEOUT` | Seconds to wait for a client to finish sending request headers before closing the connection (slowloris hygiene; positive integer) | `30` |
| `RIFT_MAX_CONNECTIONS` | Cap on concurrently-served connections per listener (positive integer). Unset means unlimited; at the cap the server stops accepting until a connection closes, so overload waits in the kernel backlog rather than piling up | unlimited |
| `RIFT_STRICT_BEHAVIORS` | Force strict mode process-wide (truthy: `1`/`true`/`yes`/`on`): a `decorate`/`shellTransform`/binary-base64-decode failure returns `500` instead of the lenient fallback body | off |
| `NO_COLOR` | Suppress ANSI color and the decorative banner in `rift-verify` / `rift-lint` output | |
| `RUST_LOG` | Detailed log configuration | `info` |

`RIFT_DISABLE_HTTP2` is an escape hatch for clients or intermediaries that mishandle HTTP/2; see
[HTTP/2 and h2c]({{ site.baseurl }}/mountebank/imposters/#http2-and-h2c). `RIFT_TCP_BACKLOG` and
`RIFT_TCP_NODELAY` are socket-tuning knobs covered under
[Performance → Runtime socket tuning]({{ site.baseurl }}/performance/#runtime-socket-tuning).

`RIFT_STRICT_BEHAVIORS` and the per-imposter `strictBehaviors` field combine with **OR** — either
being set enables strict mode. See
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
the intended hook, and (for a config) `state`-used-without-`flowState`. Exits non-zero on any
error — so a script whose entrypoint is misnamed fails here instead of at request time.

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

### healthcheck

Probe a running server's admin API and exit `0` when it answers `2xx`, `1` otherwise. This is what
the container images run as their `HEALTHCHECK` — the probe is built into the binary so the image
needs no shell and no `curl`, which is what lets the `-static` image be `FROM scratch`
(see [Docker]({{ site.baseurl }}/deployment/docker/)).

With no arguments it probes `/health` on the admin API, reading `--host`/`--port` (and therefore
`MB_HOST`/`MB_PORT`) exactly as the server does — so inside a container `rift healthcheck` needs no
configuration. A bind-any host (`0.0.0.0`, `::`) is probed on loopback, since that is where a server
bound to every interface answers.

```bash
rift-http-proxy healthcheck                                        # probes http://127.0.0.1:2525/health
MB_PORT=3000 rift-http-proxy healthcheck                           # follows MB_PORT
rift-http-proxy healthcheck --url http://localhost:9090/metrics    # probe something else
```

| Flag | Description | Default |
|:-----|:------------|:--------|
| `--url <URL>` | URL to probe instead of the admin API's `/health` | (from `--host`/`--port`) |
| `--timeout <SECONDS>` | Give up and report unhealthy after this long. Kept under the images' `HEALTHCHECK --timeout=3s` so a hung server makes the probe report the verdict itself instead of being killed mid-probe | `2` |

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
