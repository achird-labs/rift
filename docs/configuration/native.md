---
layout: default
title: Rift Extensions (_rift namespace)
parent: Configuration
nav_order: 2
---

# Rift Extensions (`_rift` Namespace)

Rift extends Mountebank's JSON configuration with advanced features through the `_rift` namespace. This allows you to use Mountebank-compatible configurations while adding Rift-specific capabilities.

---

## Overview

The `_rift` namespace can be used at two levels:

1. **Imposter level** (`_rift`): For imposter-wide settings like flow state
2. **Response level** (`_rift`): For response-specific features like fault injection and scripting

---

## Flow State

Enable stateful testing scenarios with flow state:

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
    "responses": [{
      "inject": "function(request, state) { state.count = (state.count || 0) + 1; return { statusCode: 200, body: 'Count: ' + state.count }; }"
    }]
  }]
}
```

### Flow State Backends

| Backend | Description | Use Case |
|:--------|:------------|:---------|
| `inmemory` | In-process storage (default) | Single instance, testing |
| `redis` | Redis-backed distributed storage | Multi-instance, production |

### Configuration Options

| Option | Type | Default | Description |
|:-------|:-----|:--------|:------------|
| `backend` | string | `"inmemory"` | Storage backend: inmemory or redis |
| `ttlSeconds` | integer | `300` | Time-to-live for state entries (5 minutes) |
| `redis` | object | - | Redis-specific configuration (required for redis backend) |

**Fail-loud backend errors**: an unknown `backend` string, or a `redis` backend that can't be
created (missing `redis` config, a connection/pool failure, or a binary built without the
`redis-backend` feature), fails imposter creation with `400 Bad Request` rather than silently
degrading to a no-op store. See
[Flow State → Backend configuration is fail-loud]({{ site.baseurl }}/features/flow-state/#backend-configuration-is-fail-loud)
for details. Only the *implicit* case — no `flowState` block at all — stays silent, using a no-op
store.

### Redis Configuration

When using `redis` backend:

```json
"_rift": {
  "flowState": {
    "backend": "redis",
    "ttlSeconds": 600,
    "redis": {
      "url": "redis://localhost:6379",
      "poolSize": 10,
      "keyPrefix": "rift:"
    }
  }
}
```

| Option | Type | Default | Description |
|:-------|:-----|:--------|:------------|
| `url` | string | required | Redis connection URL |
| `poolSize` | integer | `10` | Connection pool size |
| `keyPrefix` | string | `"rift:"` | Prefix for all keys (namespace isolation) |

**Connection URL formats:**

```bash
# Basic
redis://localhost:6379

# With password
redis://:password@localhost:6379

# With database selection
redis://localhost:6379/0

# TLS connection
rediss://localhost:6379

# Sentinel
redis+sentinel://localhost:26379/mymaster
```

**Key isolation example:**

```json
{
  "flowState": {
    "backend": "redis",
    "redis": {
      "url": "redis://localhost:6379",
      "keyPrefix": "rift:staging:"
    }
  }
}
```

This prefixes all keys with `rift:staging:` to isolate test environments.

### Enabling Redis Backend

Redis support requires building with the `redis-backend` feature:

```bash
# Build with Redis support
cargo build --release --features redis-backend

# Run with Redis backend
rift-http-proxy --configfile imposters.json
```

---

## Imposter Settings

Besides `flowState`, the imposter-level `_rift` block accepts these settings:

### `metrics`

Per-imposter metric emission.

```json
"_rift": {
  "metrics": { "enabled": false, "port": 9090 }
}
```

| Field | Type | Default |
|:------|:-----|:--------|
| `enabled` | boolean | `false` |
| `port` | integer | `9090` |

### `proxy`

Upstream target and connection-pool tuning for proxy responses.

```json
"_rift": {
  "proxy": {
    "upstream": { "host": "api.example.com", "port": 443, "protocol": "https" },
    "connectionPool": { "maxIdlePerHost": 100, "idleTimeoutSecs": 90 }
  }
}
```

| Field | Type | Default |
|:------|:-----|:--------|
| `upstream.host` | string | — |
| `upstream.port` | integer | — |
| `upstream.protocol` | string | `"http"` |
| `connectionPool.maxIdlePerHost` | integer | `100` |
| `connectionPool.idleTimeoutSecs` | integer | `90` |

### `scriptEngine`

Defaults for `_rift.script` execution.

```json
"_rift": {
  "scriptEngine": { "defaultEngine": "rhai", "timeoutMs": 5000 }
}
```

| Field | Type | Default | Notes |
|:------|:-----|:--------|:------|
| `defaultEngine` | string | `"rhai"` | Engine used when a script omits `engine`. |
| `timeoutMs` | integer | `5000` | Per-script wall-clock timeout. |

---

## Strict Behaviors (`strictBehaviors`)

`strictBehaviors` is a top-level imposter field — a sibling of `protocol`/`stubs`, not nested under
`_rift` — that controls whether a failing response behavior degrades quietly or fails loudly.

| Field | Type | Default | Description |
|:------|:-----|:--------|:-------------|
| `strictBehaviors` | boolean | `false` | When `true`, a `decorate`/`shellTransform`/binary-base64-decode failure returns `500` instead of silently serving the fallback body. |

```json
{
  "port": 4545,
  "protocol": "http",
  "strictBehaviors": true,
  "stubs": [{
    "responses": [{
      "is": {"statusCode": 200, "body": "Hello"},
      "_behaviors": {"decorate": "function(request, response) { throw new Error('boom'); }"}
    }]
  }]
}
```

By default (`strictBehaviors: false`), a throwing `decorate`/`shellTransform`, or a `_mode: "binary"`
response whose body isn't valid base64, still serves the stub's fallback body and only signals the
failure via an `x-rift-<behavior>-error` header. With `strictBehaviors` on, the same failure returns
`500` (still carrying that header) instead.

Strict mode can also be forced process-wide with the `RIFT_STRICT_BEHAVIORS` environment variable
(see [CLI Reference]({{ site.baseurl }}/configuration/cli/)) — the per-imposter flag **or** the env
var enables it. This is orthogonal to `RIFT_STRICT_FLOW_STORE` ([Scripting]({{ site.baseurl
}}/features/scripting/)): neither implies the other, since one governs response behaviors and the
other governs flow-store script errors.

See [Mountebank Behaviors → Error Semantics]({{ site.baseurl }}/mountebank/behaviors/#error-semantics)
for the full walkthrough of `decorate`/`shellTransform`/binary failures under strict mode.

---

## Route Patterns (`routePattern`)

`routePattern` is a **top-level stub field** — a sibling of `predicates`/`responses`/`id`/
`scenarioName`, **not** nested under `_rift` — that declares a path shape (e.g. `/users/:id`) for
extracting named path parameters from the request.

| Field | Type | Default | Description |
|:------|:-----|:--------|:-------------|
| `routePattern` | string | — | Path shape with `:name` segments to capture as path parameters. |

When the request path has the same number of `/`-separated segments as the pattern, each literal
segment must match exactly and each `:name` segment captures the corresponding path segment. The
captured values are exposed as:

- `${request.pathParams.<name>}` in response templates (see
  [Request Interpolation]({{ site.baseurl }}/mountebank/responses/#request-interpolation)).
- `request.pathParams.<name>` in scripts, for every engine (see
  [Scripting]({{ site.baseurl }}/features/scripting/)).

If `routePattern` is absent, or the request path doesn't match its segment shape, `pathParams` is
simply empty — nothing errors. Extraction is `:name`-segment matching only; there's no regex or
glob support.

```json
{
  "routePattern": "/users/:id",
  "predicates": [{ "matches": { "path": "^/users/[^/]+$" } }],
  "responses": [{ "is": { "statusCode": 200, "body": "user ${request.pathParams.id}" } }]
}
```

---

## Fault Injection

Add probabilistic fault injection to responses:

### Latency Faults

```json
{
  "is": {"statusCode": 200, "body": "OK"},
  "_rift": {
    "fault": {
      "latency": {
        "probability": 0.3,
        "minMs": 100,
        "maxMs": 500
      }
    }
  }
}
```

Or with fixed delay:

```json
"_rift": {
  "fault": {
    "latency": {
      "probability": 1.0,
      "ms": 200
    }
  }
}
```

### Error Faults

```json
{
  "is": {"statusCode": 200, "body": "OK"},
  "_rift": {
    "fault": {
      "error": {
        "probability": 0.1,
        "status": 503,
        "body": "Service Unavailable",
        "headers": {
          "Retry-After": "60"
        }
      }
    }
  }
}
```

### TCP Faults

`tcp` is a **string** naming a connection-level fault (not an object). When it fires, the connection
is disrupted at the transport level instead of an HTTP response being sent:

```json
"_rift": {
  "fault": {
    "tcp": "CONNECTION_RESET_BY_PEER"
  }
}
```

TCP fault types (canonical name or short alias):

| Value | Aliases | Effect |
|:------|:--------|:-------|
| `CONNECTION_RESET_BY_PEER` | `reset` | Real TCP reset (RST) |
| `EMPTY_RESPONSE` | `empty` | Close with no bytes sent |
| `RANDOM_DATA_THEN_CLOSE` | `random`, `garbage` | Write random bytes, then close |
| `MALFORMED_RESPONSE_CHUNK` | `malformed` | Status line + malformed chunked body, then close |

When `latency`, `tcp`, and `error` are combined in one `fault` block, `tcp` takes precedence over
`error`. See [Fault Injection]({{ site.baseurl }}/features/fault-injection/) for precedence, the
top-level `fault` response form, and scripted faults.

---

## Scripting

`_rift.script` runs a script (engine `rhai`, `lua`, or `javascript`) that decides whether to inject a
response. The script defines `should_inject(request, flow_store)` and returns a map with an `inject`
flag; when `inject` is true it also carries `fault`/`status`/`body`/`headers`. The `flow_store` handle
is keyed by `(flow_id, key)`.

```json
{
  "_rift": {
    "flowState": { "backend": "inmemory", "ttlSeconds": 300 },
    "script": {
      "engine": "rhai",
      "code": "fn should_inject(request, flow_store) { let n = flow_store.increment(\"demo\", \"count\"); #{ inject: true, fault: \"error\", status: 200, body: `count ${n}` } }"
    }
  }
}
```

- `rhai` is built in; `lua` requires the `lua` feature; `javascript` requires the `javascript`
  feature. JavaScript can also use the Mountebank `inject` response format directly.
- Scripts require `--allow-injection` and are bounded by a wall-clock timeout
  (`_rift.scriptEngine.timeoutMs`, default 5000 ms).

See [Scripting]({{ site.baseurl }}/features/scripting/) for the full API (request object, `flow_store`
methods, `last_error()`, return values) and [Flow State]({{ site.baseurl }}/features/flow-state/) for
the state model.

---

## Complete Example

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
  "stubs": [
    {
      "predicates": [{"equals": {"path": "/api/users"}}],
      "responses": [{
        "is": {
          "statusCode": 200,
          "headers": {"Content-Type": "application/json"},
          "body": "{\"users\": []}"
        },
        "_rift": {
          "fault": {
            "latency": {
              "probability": 0.2,
              "minMs": 50,
              "maxMs": 200
            }
          }
        }
      }]
    },
    {
      "predicates": [{"equals": {"method": "POST", "path": "/api/orders"}}],
      "responses": [{
        "is": {"statusCode": 201, "body": "Created"},
        "_rift": {
          "fault": {
            "error": {
              "probability": 0.05,
              "status": 503,
              "body": "Service temporarily unavailable"
            }
          }
        }
      }]
    },
    {
      "predicates": [{"equals": {"path": "/api/counter"}}],
      "responses": [{
        "_rift": {
          "script": {
            "engine": "rhai",
            "code": "let count = flow.get('requests').unwrap_or(0) + 1; flow.set('requests', count); #{ statusCode: 200, body: `Request #${count}` }"
          }
        }
      }]
    }
  ]
}
```

---

## Combining with Mountebank Features

`_rift` extensions work alongside standard Mountebank features:

```json
{
  "is": {
    "statusCode": 200,
    "body": "Hello"
  },
  "_behaviors": {
    "wait": 50,
    "decorate": "function(request, response) { response.body += ' World'; }"
  },
  "_rift": {
    "fault": {
      "latency": {
        "probability": 0.1,
        "ms": 100
      }
    }
  }
}
```

Both `_behaviors.wait` and `_rift.fault.latency` will be applied.

---

## See Also

- [Mountebank Compatibility](mountebank.md) - Standard Mountebank configuration
- [Fault Injection](../features/fault-injection.md) - Detailed fault injection documentation
- [Scripting](../features/scripting.md) - Scripting engine documentation
