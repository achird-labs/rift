---
layout: default
title: Configuration
nav_order: 4
has_children: true
permalink: /configuration/
---

# Configuration

Rift uses Mountebank-compatible JSON configuration with optional `_rift` extensions for advanced features.

---

## Mountebank Format

Use the standard Mountebank JSON format for creating imposters:

```json
{
  "imposters": [
    {
      "port": 4545,
      "protocol": "http",
      "stubs": [
        {
          "predicates": [{ "equals": { "path": "/api/users" } }],
          "responses": [{ "is": { "statusCode": 200, "body": "[]" } }]
        }
      ]
    }
  ]
}
```

Load at startup:

```bash
docker run -v $(pwd)/imposters.json:/imposters.json \
  zainalpour/rift-proxy:latest --configfile /imposters.json
```

### Top-level keys

| Key | Purpose |
|---|---|
| `imposters` | The imposters to create — the Mountebank format above. |
| `intercept` | *Optional, Rift extension.* Declares the [HTTPS intercept listener]({{ site.baseurl }}/features/intercept-proxy/#declare-it-in-the-config-file) and its rules, so a container needs no post-boot admin call to install them. Its keys are `host`, `port`, the CA pair (`caCertPath`/`caKeyPath` **or** `caCertPem`/`caKeyPem`), `rules`, and `auth`; any other key is a startup error. `returnCaKey: true` is refused too: a config file has no response to return a generated key in, so use `POST /intercept` for that. |
| `routes` | *Optional, Rift extension.* The [front door]({{ site.baseurl }}/features/front-door/) route table, as `{"routes": [ ... ]}`. Validated at load, so a table that cannot route is a startup error. It only takes effect with `--front-door`. |

`intercept.auth` (issue #878) is `{"username": "…", "password": "…"}` and requires
`Proxy-Authorization: Basic …` on every `CONNECT` to the listener. Omit it and the proxy is open —
see [Authenticating the proxy]({{ site.baseurl }}/features/intercept-proxy/#authenticating-the-proxy)
for why that matters on a shared host. A blank username or password is a startup error, not a
disabled gate. Certificate and CA handling is covered in [TLS/HTTPS]({{ site.baseurl }}/features/tls/).

Any other top-level key is ignored.

### Document shapes and formats

A document is read as JSON when it starts with `{` or `[`, and as YAML otherwise. The accepted
shapes are:

| Shape | JSON | YAML |
|---|---|---|
| `{"imposters": [...]}` wrapper (may carry `intercept` / `routes`) | yes | no |
| A single imposter object (`{"port": 4545, ...}`) | yes | no |
| A bare array (sequence) of imposters | yes | yes — the only YAML shape |

So a YAML config file is always a sequence at the root, even for one imposter. Only the wrapper has
somewhere to put an `intercept` or `routes` block; declaring one in a single-imposter JSON document
is a startup error naming the fix rather than a block that silently does nothing.

Before parsing, a `--configfile` document is run through the EJS subset Mountebank config files use:
`<% include 'file' %>`, `<%- stringify('file') %>` (paths relative to the document) and
`<%= process.env.VAR %>` / `<%= process.env.VAR || 'default' %>`. Any other `<% %>` tag is refused
with an error rather than stripped; pass `--no-parse` for a document that holds a literal `<%`. A
referenced variable that is unset renders empty and logs a warning naming it. See
[CLI Reference → Imposter Sources]({{ site.baseurl }}/configuration/cli/#imposter-sources) for what a
document fetched over `https:` may not do, and
[Data directory]({{ site.baseurl }}/configuration/cli/#data-directory---datadir) for the stricter
rules on `--datadir` files.

An imposter with no `port`, or `"port": 0`, gets a free port assigned; imposters that declare a port
are always created first.

Or create dynamically via API:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @imposter.json
```

[Full Mountebank Format Reference]({{ site.baseurl }}/configuration/mountebank/)

---

## Rift Extensions (`_rift` namespace)

Extend Mountebank configurations with advanced chaos engineering features:

```json
{
  "port": 4545,
  "protocol": "http",
  "_rift": {
    "flowState": {
      "backend": "inmemory",
      "ttlSeconds": 300
    }
  },
  "stubs": [{
    "predicates": [{ "equals": { "path": "/api/users" } }],
    "responses": [{
      "is": { "statusCode": 200, "body": "[]" },
      "_rift": {
        "fault": {
          "latency": {
            "probability": 0.3,
            "minMs": 100,
            "maxMs": 500
          }
        }
      }
    }]
  }]
}
```

Available `_rift` features:
- **Flow State**: Stateful testing with in-memory or Redis backends
- **Fault Injection**: Probabilistic latency, error, and TCP faults
- **Scripting**: `respond(ctx)` scripts in Rhai or JavaScript
- **Templating and state operations**: `_rift.templated` (function-grammar response templates) and `_rift.stateOps` (declarative flow-state writes)

[Full Rift Extensions Reference]({{ site.baseurl }}/configuration/native/)

---

## Environment Variables

Configure Rift behavior via environment variables:

| Variable | Description | Default |
|:---------|:------------|:--------|
| `MB_PORT` | Admin API port | `2525` |
| `MB_HOST` | Admin API bind IP address (IPv4, or IPv6 `::1` / `[::1]`) | `0.0.0.0` |
| `MB_CONFIGFILE` | Imposter config file | |
| `MB_DATADIR` | Persistent storage directory | |
| `MB_ALLOW_INJECTION` | Enable JavaScript injection | `false` |
| `MB_LOCAL_ONLY` | Localhost only | `false` |
| `MB_LOGLEVEL` | Log level: `trace`, `debug`, `info`, `warn`, `error` | `info` |
| `MB_APIKEY` | Admin API key | |
| `RIFT_METRICS_PORT` | Prometheus metrics port | `9090` |
| `RUST_LOG` | Full `tracing` filter; overrides `MB_LOGLEVEL` when set | unset |

This is the common subset; the full list — TLS, intercept, runtime and socket-tuning variables —
is in the [CLI Reference]({{ site.baseurl }}/configuration/cli/#environment-variables).

```bash
docker run -e MB_PORT=2525 -e MB_ALLOW_INJECTION=true \
  -e MB_LOGLEVEL=debug zainalpour/rift-proxy:latest
```

---

## Command Line Options

```bash
rift [OPTIONS]

Options:
      --port <PORT>          Admin API port [default: 2525]
      --host <HOST>          Admin API bind IP address (IPv4, or IPv6 `::1` / `[::1]`) [default: 0.0.0.0]
      --configfile <FILE>    Load imposters from a JSON or YAML file
      --imposters <URI,...>  Load imposters from file: paths or https:// URLs
      --datadir <DIR>        Persist admin-API imposters as <DIR>/<port>.json
      --allow-injection      Enable JavaScript injection and scripts
      --local-only           Bind the admin API and /metrics to loopback
      --api-key <TOKEN>      Require this token on every admin API request
      --rcfile <FILE>        Read defaults from a Mountebank-style JSON rcfile
      --loglevel <LEVEL>     trace, debug, info, warn, error [default: info]
      --metrics-port <PORT>  Prometheus metrics port [default: 9090]
      --no-parse             Load --configfile verbatim, without EJS rendering
  -h, --help                 Print help
  -V, --version              Print version
```

This is an excerpt. Subcommands (`save`, `replay`, `stop`, `restart`, `script`, `healthcheck`),
TLS and intercept flags, and the `--rcfile` key list are in the full reference.

[Full CLI Reference]({{ site.baseurl }}/configuration/cli/)

---

## Use Cases

### Standard API Mocking
Use Mountebank JSON format for:
- Migrating from Mountebank
- Creating API mocks for integration tests
- Working with existing Mountebank tooling
- Service virtualization

### Advanced Chaos Engineering
Add `_rift` extensions for:
- Probabilistic fault injection
- Stateful testing scenarios
- Complex conditional logic with scripting
- Distributed state with Redis backend
