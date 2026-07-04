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
supports the same shape, and a top-level fault now **resets the connection at the transport level**
rather than returning an HTTP 502 (Mountebank parity, issue #362):

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

> **Behavior change:** before v0.8.0 a top-level `fault` returned a framed HTTP `502`. It now
> performs a real connection reset/close, matching Mountebank's transport-fault semantics.

### Fault Precedence

When a single `_rift.fault` block combines `latency`, `tcp`, and `error`, they are evaluated in
this order:

1. **`latency`** — applied first (the response is delayed), then evaluation continues.
2. **`tcp`** — if it fires, the connection is reset and no HTTP response is sent. A `tcp` fault
   is a transport-level event, so it takes precedence over `error`.
3. **`error`** — applied only when no `tcp` fault fired.

So `latency` + `tcp` is a *delay-then-drop*, and a configured `tcp` fault always wins over an
`error` fault rather than being silently dropped.

---

## Scripted Faults

For dynamic fault injection based on request data or state, use the scripting feature.

### Rhai Script - Retry Simulation

Fail the first 2 requests, pass through on the 3rd:

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
          "code": "let flow_id = request.headers[\"x-flow-id\"]; if flow_id == () { flow_id = \"default\"; }; let attempts = flow_store.get(flow_id, \"attempts\"); if attempts == () { attempts = 0; }; attempts += 1; flow_store.set(flow_id, \"attempts\", attempts); if attempts <= 2 { #{inject: true, fault: \"error\", status: 503, body: `{\"error\":\"Temporary failure\",\"attempt\":${attempts}}`, headers: #{\"Content-Type\": \"application/json\"}} } else { #{inject: false} }"
        }
      }
    }]
  }]
}
```

### Lua Script - Rate Limiting

```json
{
  "_rift": {
    "flowState": {"backend": "inmemory", "ttlSeconds": 60}
  },
  "stubs": [{
    "responses": [{
      "_rift": {
        "script": {
          "engine": "lua",
          "code": "function should_inject(request, flow_store)\n  local fid = 'ratelimit'\n  local count = flow_store:increment(fid, 'requests')\n  if count > 100 then\n    return {\n      inject = true,\n      fault = 'error',\n      status = 429,\n      body = '{\"error\":\"Rate limit exceeded\"}',\n      headers = {['Content-Type'] = 'application/json', ['Retry-After'] = '60'}\n    }\n  end\n  return {inject = false}\nend"
        }
      }
    }]
  }]
}
```

### Script Return Values

Scripts must return a map/table with an `inject` flag:

**Rhai:**
```rhai
// No injection - pass through
#{ inject: false }

// Inject error
#{
  inject: true,
  fault: "error",
  status: 503,
  body: "{\"error\": \"Service unavailable\"}",
  headers: #{ "Content-Type": "application/json" }
}

// Inject latency
#{
  inject: true,
  fault: "latency",
  duration_ms: 500
}
```

**Lua:**
```lua
-- No injection
return { inject = false }

-- Inject error
return {
  inject = true,
  fault = "error",
  status = 503,
  body = '{"error": "Service unavailable"}',
  headers = { ["Content-Type"] = "application/json" }
}

-- Inject latency
return {
  inject = true,
  fault = "latency",
  duration_ms = 500
}
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
    "flowState": {"backend": "inmemory", "ttlSeconds": 300}
  },
  "stubs": [{
    "predicates": [{ "equals": { "path": "/api/resource" } }],
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "let flow_id = request.headers[\"x-request-id\"]; if flow_id == () { flow_id = \"default\"; }; let attempts = flow_store.increment(flow_id, \"attempts\"); if attempts <= 2 { #{inject: true, fault: \"error\", status: 503, body: `{\"error\":\"Retry later\",\"attempt\":${attempts}}`, headers: #{\"Content-Type\": \"application/json\", \"Retry-After\": \"1\"}} } else { #{inject: false} }"
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
