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
| `intercept` | *Optional, Rift extension.* Declares the [HTTPS intercept listener](../features/intercept-proxy.md#declare-it-in-the-config-file) and its rules, so a container needs no post-boot admin call to install them. |

A file may also be a single imposter object (`{"port": 4545, ...}`) or a bare array of them; those
shapes have nowhere to put an `intercept` block, so declaring one there is a startup error naming
the fix rather than a block that silently does nothing.

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
- **Scripting**: Multi-engine scripting (Rhai, JavaScript)

[Full Rift Extensions Reference]({{ site.baseurl }}/configuration/native/)

---

## Environment Variables

Configure Rift behavior via environment variables:

| Variable | Description | Default |
|:---------|:------------|:--------|
| `MB_PORT` | Admin API port | `2525` |
| `MB_HOST` | Bind hostname | `0.0.0.0` |
| `MB_CONFIGFILE` | Imposter config file | |
| `MB_DATADIR` | Persistent storage directory | |
| `MB_ALLOW_INJECTION` | Enable JavaScript injection | `false` |
| `MB_LOCAL_ONLY` | Localhost only | `false` |
| `MB_LOGLEVEL` | Log level | `info` |
| `RIFT_METRICS_PORT` | Prometheus metrics port | `9090` |
| `RUST_LOG` | Detailed log configuration | `info` |

```bash
docker run -e MB_PORT=2525 -e MB_ALLOW_INJECTION=true \
  -e RUST_LOG=debug zainalpour/rift-proxy:latest
```

---

## Command Line Options

```bash
rift-http-proxy [OPTIONS]

Options:
      --port <PORT>          Admin API port [default: 2525]
      --host <HOST>          Bind hostname [default: 0.0.0.0]
      --configfile <FILE>    Load imposters from JSON file
      --datadir <DIR>        Persistent storage directory
      --allow-injection      Enable JavaScript injection
      --local-only           Localhost only
      --loglevel <LEVEL>     Log level [default: info]
      --metrics-port <PORT>  Prometheus metrics port [default: 9090]
  -h, --help                 Print help
  -V, --version              Print version
```

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
