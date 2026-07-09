---
layout: default
title: Fault Injection
parent: Features
nav_order: 1
---

# Fault Injection

Rift enables fault injection for chaos engineering and resilience testing.

---

## Mountebank Behaviors

### Latency with wait Behavior

```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/api/slow" } }],
    "responses": [{
      "is": { "statusCode": 200, "body": "OK" },
      "_behaviors": { "wait": 2000 }
    }]
  }]
}
```

### Random Latency

```json
{
  "_behaviors": {
    "wait": {
      "inject": "function() { return Math.floor(Math.random() * 1000) + 500; }"
    }
  }
}
```

### Error Responses

```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/api/error" } }],
    "responses": [{
      "is": {
        "statusCode": 500,
        "body": { "error": "Internal Server Error" }
      }
    }]
  }]
}
```

### Probabilistic Errors with Injection

```json
{
  "stubs": [{
    "responses": [{
      "inject": "function(config) { if (Math.random() < 0.1) { return { statusCode: 500, body: 'Random failure' }; } return { statusCode: 200, body: 'Success' }; }"
    }]
  }]
}
```

---

## Rift Extensions (`_rift.fault`)

### Probabilistic Latency

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [{
    "predicates": [{ "startsWith": { "path": "/api" } }],
    "responses": [{
      "is": { "statusCode": 200, "body": "OK" },
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

### Fixed Latency

```json
{
  "_rift": {
    "fault": {
      "latency": {
        "probability": 1.0,
        "ms": 1000
      }
    }
  }
}
```

### Probabilistic Errors

```json
{
  "stubs": [{
    "predicates": [{ "equals": { "method": "POST" } }],
    "responses": [{
      "is": { "statusCode": 200, "body": "OK" },
      "_rift": {
        "fault": {
          "error": {
            "probability": 0.1,
            "status": 503,
            "body": "{\"error\": \"Service Unavailable\"}",
            "headers": {
              "Retry-After": "30"
            }
          }
        }
      }
    }]
  }]
}
```

### Combined Faults

Apply both latency and errors:

```json
{
  "responses": [{
    "is": { "statusCode": 200, "body": "OK" },
    "_rift": {
      "fault": {
        "latency": {
          "probability": 0.5,
          "minMs": 200,
          "maxMs": 1000
        },
        "error": {
          "probability": 0.05,
          "status": 500
        }
      }
    }
  }]
}
```

### TCP Faults

Simulate network-level failures:

```json
{
  "_rift": {
    "fault": {
      "tcp": "CONNECTION_RESET_BY_PEER"
    }
  }
}
```

TCP fault types (each accepts a WireMock-style canonical name or a short alias):

| Fault | Aliases | Effect |
|:------|:--------|:-------|
| `CONNECTION_RESET_BY_PEER` | `reset` | Real TCP reset (RST) — the connection is aborted |
| `EMPTY_RESPONSE` | `empty` | Close the connection with no bytes sent |
| `RANDOM_DATA_THEN_CLOSE` | `random`, `garbage` | Write random bytes, then close |
| `MALFORMED_RESPONSE_CHUNK` | `malformed` | Send a status line + a malformed chunked body, then close |

These are real transport-level events applied beneath TLS, so HTTPS imposters get a genuine socket
fault too. Because a connection-level fault aborts the whole socket, an imposter that uses any TCP
fault is served over **HTTP/1 only** (HTTP/2 multiplexing is incompatible with mid-stream connection
aborts).

---

## Top-Level Fault Response (Mountebank Parity)

Mountebank lets a stub response be a bare `fault` instead of an `is`/`proxy`/`inject` body. Rift
supports the same shape, and a top-level fault **resets the connection at the transport level**,
matching Mountebank's transport-fault semantics:

```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/api/flaky" } }],
    "responses": [{
      "fault": "CONNECTION_RESET_BY_PEER"
    }]
  }]
}
```

A client hitting this stub sees a transport error (connection reset), not a response. The recognized
fault strings are the same set as `_rift.fault.tcp` above (`CONNECTION_RESET_BY_PEER`,
`EMPTY_RESPONSE`, `RANDOM_DATA_THEN_CLOSE`, `MALFORMED_RESPONSE_CHUNK`, plus their aliases). An
unrecognized fault string is a configuration error and yields `500 Unknown fault: <value>`.

Like `_rift.fault.tcp`, a bare top-level `fault` also forces the whole imposter onto **HTTP/1
only**: it is a connection-level event, and aborting a socket mid-stream is incompatible with
HTTP/2 multiplexing — so an imposter with even one stub returning a top-level `fault` never
negotiates HTTP/2, regardless of what its other stubs do.

### Fault Precedence

When a single `_rift.fault` block combines `latency`, `tcp`, and `error`, they are evaluated in
this order:

1. **`latency`** — applied first (the response is delayed), then evaluation continues.
2. **`tcp`** — if it fires, the connection is reset and no HTTP response is sent. A `tcp` fault
   is a transport-level event, so it takes precedence over `error`.
3. **`error`** — applied only when no `tcp` fault fired.

So `latency` + `tcp` is a *delay-then-drop*, and a configured `tcp` fault always wins over an
`error` fault rather than being silently dropped.

### Response-type precedence

`_rift.fault` and a top-level `fault` string live on different response shapes, and a single stub
response can only render as one shape. When a response carries more than one recognized type, Rift
picks in this order: **`is` > `proxy` > `inject` > `fault`**. This response-type precedence means a
stub combining an `is` block (with `_rift.fault` inside it) *and* a top-level `"fault"` string on
the same response silently drops the top-level `fault` — `is` wins and the fault string is never
evaluated. If you want the top-level transport fault, the response must be a bare `fault` with no
`is`/`proxy`/`inject` alongside it.

---

## Scripted Faults

For dynamic fault injection based on request data or state, use the scripting feature. Full
reference (the unified `ctx` object, result constructors, entrypoint placement) lives on the
[Scripting](./scripting.md#ctx-api) page; this section just shows it applied to fault injection.

### Rhai Script - Retry Simulation

Fail the first 2 requests, pass through on the 3rd. Bare-expression form: the whole script body is
the `respond(ctx)` function, with `ctx` already in scope, and `http(status, body)` replaces a
hand-built `#{ inject:, fault: }` map:

```json
{
  "port": 4545,
  "protocol": "http",
  "_rift": {
    "flowState": {"backend": "inmemory", "ttlSeconds": 300}
  },
  "stubs": [{
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "let n = ctx.state.incr(\"attempts\"); if n <= 2 { http(503, #{ error: \"Temporary failure\", attempt: n }) } else { pass() }"
        }
      }
    }]
  }]
}
```

### Script Return Values

`respond(ctx)` returns a result constructor — see [Scripting](./scripting.md#result-constructors)
for the full reference. The shapes used above, restated:

**Rhai:**
```rhai
// No injection - pass through
pass()

// Inject error
http(503, #{ error: "Service unavailable" })

// Inject latency
delay(500)
```

---

## Use Cases

### Testing Timeout Handling

```json
{
  "predicates": [{ "equals": { "path": "/api/external-service" } }],
  "responses": [{
    "is": { "statusCode": 200, "body": "OK" },
    "_rift": {
      "fault": {
        "latency": {
          "probability": 1.0,
          "ms": 35000
        }
      }
    }
  }]
}
```

### Testing Retry Logic

Use scripting to fail a specific number of requests before succeeding:

```json
{
  "port": 4545,
  "protocol": "http",
  "_rift": {
    "flowState": {"backend": "inmemory", "ttlSeconds": 300, "flowIdSource": "header:X-Request-Id"}
  },
  "stubs": [{
    "predicates": [{ "equals": { "path": "/api/resource" } }],
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "let attempts = ctx.state.incr(\"attempts\"); if attempts <= 2 { http(503, #{ error: \"Retry later\", attempt: attempts }).header(\"Retry-After\", \"1\") } else { pass() }"
        }
      }
    }]
  }]
}
```

### Testing Circuit Breaker

Simulate random failures:

```json
{
  "_rift": {
    "fault": {
      "error": {
        "probability": 0.5,
        "status": 500,
        "body": "{\"error\": \"Service failure\"}"
      }
    }
  }
}
```

---

## Best Practices

1. **Start with low probability** - Begin at 1-5% and increase gradually
2. **Use specific matches** - Target specific endpoints, not all traffic
3. **Add identifiers** - Include headers to identify injected faults
4. **Monitor metrics** - Track fault injection rate and impact
5. **Test in staging first** - Validate fault scenarios before production
6. **Document scenarios** - Keep a runbook of chaos experiments
7. **Use flow_id for isolation** - Namespace state by request/user ID to avoid cross-contamination
