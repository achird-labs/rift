---
layout: default
title: Responses
parent: Mountebank Compatibility
nav_order: 3
---

# Responses

Responses define what an imposter returns when a stub's predicates match.

---

## Response Types

### is (Static Response)

Return a fixed response:

```json
{
  "is": {
    "statusCode": 200,
    "headers": {
      "Content-Type": "application/json",
      "X-Custom-Header": "value"
    },
    "body": {
      "message": "Success",
      "data": { "id": 1 }
    }
  }
}
```

### proxy (Forward Request)

Forward requests to a real server and optionally record responses:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyAlways",
    "predicateGenerators": [{
      "matches": { "path": true, "method": true }
    }]
  }
}
```

### inject (Dynamic Response)

Generate responses with JavaScript:

```json
{
  "inject": "function(request, state, logger) { return { statusCode: 200, body: 'Request path: ' + request.path }; }"
}
```

---

## Static Responses (is)

### Status Codes

```json
{ "is": { "statusCode": 201 } }
{ "is": { "statusCode": 400 } }
{ "is": { "statusCode": 500 } }
```

Status codes can also be specified as strings for compatibility with some tools:

```json
{ "is": { "statusCode": "200" } }
{ "is": { "statusCode": "404" } }
```

### Headers

```json
{
  "is": {
    "statusCode": 200,
    "headers": {
      "Content-Type": "application/json",
      "Cache-Control": "no-cache",
      "X-Request-Id": "abc123"
    }
  }
}
```

### Body Types

**String body:**
```json
{ "is": { "body": "Hello, World!" } }
```

**JSON body (auto-serialized):**
```json
{
  "is": {
    "body": {
      "users": [
        { "id": 1, "name": "Alice" },
        { "id": 2, "name": "Bob" }
      ]
    }
  }
}
```

**XML body:**
```json
{
  "is": {
    "headers": { "Content-Type": "application/xml" },
    "body": "<?xml version=\"1.0\"?><user><id>1</id></user>"
  }
}
```

**Binary body (base64):**
```json
{
  "is": {
    "body": "SGVsbG8gV29ybGQ=",
    "_mode": "binary"
  }
}
```

The `body` is standard base64 (with padding); `_mode: "binary"` tells Rift to decode it before
serving. Omit `_mode` (or set `"text"`) for a normal text/JSON body.

---

## Request Interpolation

Static (`is`) responses can echo values from the incoming request using `${request.…}` tokens. Rift
substitutes them into the response **body** and **header values** before any behaviors run:

| Token | Resolves to |
|:------|:------------|
| `${request.path}` | Request path |
| `${request.method}` | HTTP method |
| `${request.body}` | Raw request body |
| `${request.query.<name>}` | Query parameter `<name>` |
| `${request.headers.<name>}` | Request header `<name>` (case-insensitive) |
| `${request.pathParams.<name>}` | Path parameter `<name>` |

```json
{
  "is": {
    "statusCode": 200,
    "headers": { "X-Echo-Path": "${request.path}" },
    "body": "You called ${request.method} ${request.path} with q=${request.query.q}"
  }
}
```

A request `GET /search?q=rust` returns `You called GET /search with q=rust` and an
`X-Echo-Path: /search` header.

These `${request.…}` tokens are distinct from the free-form `${name}` placeholders that the
[`copy` and `lookup` behaviors]({{ site.baseurl }}/mountebank/behaviors/#copy) fill in — the two do
not collide, because only tokens beginning with `request.` are treated as request interpolation. On
the proxy path, only the body is interpolated (not headers).

---

## Response Cycling

When a stub has multiple responses, they cycle through in round-robin order. Each request returns the next response in the sequence, wrapping back to the first after reaching the end.

### Basic Cycling

```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/cycle" } }],
    "responses": [
      { "is": { "statusCode": 200, "body": "Response 1" } },
      { "is": { "statusCode": 200, "body": "Response 2" } },
      { "is": { "statusCode": 200, "body": "Response 3" } }
    ]
  }]
}
```

Requests return responses in order:
- Request 1 → "Response 1"
- Request 2 → "Response 2"
- Request 3 → "Response 3"
- Request 4 → "Response 1" (cycles back)
- Request 5 → "Response 2"
- ...

### Use Cases

**Simulating intermittent failures:**
```json
{
  "responses": [
    { "is": { "statusCode": 200, "body": "OK" } },
    { "is": { "statusCode": 200, "body": "OK" } },
    { "is": { "statusCode": 503, "body": "Service Unavailable" } }
  ]
}
```

Every third request fails - useful for testing retry logic.

**Simulating state changes:**
```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/order/status" } }],
    "responses": [
      { "is": { "body": { "status": "pending" } } },
      { "is": { "body": { "status": "processing" } } },
      { "is": { "body": { "status": "shipped" } } },
      { "is": { "body": { "status": "delivered" } } }
    ]
  }]
}
```

Each poll returns the next order status.

**Returning different data:**
```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/random-quote" } }],
    "responses": [
      { "is": { "body": { "quote": "Be the change you wish to see." } } },
      { "is": { "body": { "quote": "Stay hungry, stay foolish." } } },
      { "is": { "body": { "quote": "Think different." } } }
    ]
  }]
}
```

### Cycling with Repeat Behavior

Use the `repeat` behavior to return the same response multiple times before advancing:

```json
{
  "responses": [
    {
      "is": { "statusCode": 200, "body": "Success" },
      "_behaviors": { "repeat": 3 }
    },
    { "is": { "statusCode": 500, "body": "Error" } }
  ]
}
```

This returns "Success" three times, then "Error" once, then cycles:
- Requests 1-3 → "Success"
- Request 4 → "Error"
- Requests 5-7 → "Success"
- Request 8 → "Error"
- ...

See [Behaviors]({{ site.baseurl }}/mountebank/behaviors/#repeat) for more on the repeat behavior.

### Cycling State

- Cycling state is **per-stub** - each stub maintains its own position
- State **resets** when the imposter is deleted and recreated
- State is **not persisted** - restarting Rift resets all cycling positions

### Mixed Response Types

Cycling works with any response type - you can mix `is`, `proxy`, and `inject`:

```json
{
  "responses": [
    { "is": { "statusCode": 200, "body": "Cached response" } },
    { "proxy": { "to": "https://api.example.com" } }
  ]
}
```

First request returns cached data, second proxies to real API, then cycles.

---

## Rift Extensions: Controlled State Management

{: .rift-only }
> **Rift-Specific Feature**: The following features use Rift's `_rift.script` and `flowState` extensions, which are not available in Mountebank.

Standard response cycling is **global** - all users share the same position in the cycle. This can cause unpredictable behavior in multi-user scenarios. Rift provides **flow state** and **scripting** for controlled, isolated state management.

### Comparison: Cycling vs Flow State

| Capability | Mountebank Cycling | Rift Flow State |
|:-----------|:-------------------|:----------------|
| State scope | Global (all users) | Per flow_id (isolated) |
| State persistence | Lost on restart | Redis backend available |
| Complex logic | Not possible | Full scripting support |
| Time-based rules | Not possible | TTL + timestamp checks |
| Per-user tracking | Not possible | Use user ID as flow_id |

### Per-User Retry Simulation

With standard cycling, if User A triggers the first failure, User B gets the second failure. With Rift flow state, each user gets their own retry sequence:

```json
{
  "port": 4545,
  "protocol": "http",
  "_rift": {
    "flowState": { "backend": "inmemory", "ttlSeconds": 300 }
  },
  "stubs": [{
    "predicates": [{ "equals": { "path": "/api/resource" } }],
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "fn should_inject(request, flow_store) { let user_id = request.headers[\"x-user-id\"]; if user_id == () { user_id = \"anonymous\"; }; let attempts = flow_store.increment(user_id, \"attempts\"); if attempts <= 2 { #{inject: true, fault: \"error\", status: 503, body: `{\"error\":\"Temporary failure\",\"attempt\":${attempts},\"user\":\"${user_id}\"}`, headers: #{\"Content-Type\": \"application/json\", \"Retry-After\": \"1\"}} } else { #{inject: false} } }"
        }
      },
      "is": { "statusCode": 200, "body": "{\"status\": \"success\"}" }
    }]
  }]
}
```

Now each user experiences their own retry sequence:
- User A: Fail → Fail → Success
- User B: Fail → Fail → Success (independent of User A)

### Time-Window Rate Limiting

{: .rift-only }
> **Rift-Only**: Mountebank cannot implement time-based rate limiting. Cycling only counts requests, not time.

Limit requests per time window with automatic reset:

```json
{
  "port": 4545,
  "protocol": "http",
  "_rift": {
    "flowState": { "backend": "inmemory", "ttlSeconds": 60 }
  },
  "stubs": [{
    "predicates": [{ "startsWith": { "path": "/api" } }],
    "responses": [{
      "_rift": {
        "script": {
          "engine": "lua",
          "code": "function should_inject(request, flow_store)\n  local client_ip = request.headers['x-forwarded-for'] or 'default'\n  local window_key = client_ip .. ':' .. math.floor(os.time() / 60)\n  local count = flow_store:increment(window_key, 'requests')\n  flow_store:set_ttl(window_key, 60)\n  \n  if count > 100 then\n    return {\n      inject = true,\n      fault = 'error',\n      status = 429,\n      body = '{\"error\":\"Rate limit: 100 requests per minute\",\"count\":' .. count .. '}',\n      headers = {\n        ['Content-Type'] = 'application/json',\n        ['Retry-After'] = tostring(60 - (os.time() % 60)),\n        ['X-RateLimit-Limit'] = '100',\n        ['X-RateLimit-Remaining'] = '0'\n      }\n    }\n  end\n  return { inject = false }\nend"
        }
      },
      "is": { "statusCode": 200, "body": "{\"status\": \"ok\"}" }
    }]
  }]
}
```

Features not possible with Mountebank:
- Rate limit resets every 60 seconds automatically
- Per-client tracking via IP or header
- Dynamic `Retry-After` header with remaining time
- `X-RateLimit-*` headers with actual counts

### Quota Exhaustion with Reset

{: .rift-only }
> **Rift-Only**: Track quota consumption over time with manual or automatic reset.

```json
{
  "_rift": {
    "flowState": { "backend": "inmemory", "ttlSeconds": 86400 }
  },
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/api/expensive-operation" } }],
      "responses": [{
        "_rift": {
          "script": {
            "engine": "rhai",
            "code": "fn should_inject(request, flow_store) { let api_key = request.headers[\"x-api-key\"]; if api_key == () { return #{inject: true, fault: \"error\", status: 401, body: \"{\\\"error\\\":\\\"API key required\\\"}\", headers: #{\"Content-Type\": \"application/json\"}}; }; let used = flow_store.get(api_key, \"quota_used\"); if used == () { used = 0; }; let limit = 1000; if used >= limit { #{inject: true, fault: \"error\", status: 402, body: `{\"error\":\"Quota exceeded\",\"used\":${used},\"limit\":${limit}}`, headers: #{\"Content-Type\": \"application/json\"}} } else { flow_store.set(api_key, \"quota_used\", used + 1); #{inject: false} } }"
          }
        },
        "is": { "statusCode": 200, "body": "{\"result\": \"expensive computation\"}" }
      }]
    },
    {
      "predicates": [{ "equals": { "method": "POST", "path": "/api/quota/reset" } }],
      "responses": [{
        "_rift": {
          "script": {
            "engine": "rhai",
            "code": "fn should_inject(request, flow_store) { let api_key = request.headers[\"x-api-key\"]; if api_key == () { api_key = \"default\"; }; flow_store.delete(api_key, \"quota_used\"); #{inject: true, fault: \"error\", status: 200, body: \"{\\\"message\\\":\\\"Quota reset\\\"}\", headers: #{\"Content-Type\": \"application/json\"}} }"
          }
        }
      }]
    }
  ]
}
```

### Circuit Breaker Pattern

{: .rift-only }
> **Rift-Only**: Implement circuit breaker with failure counting, open/closed states, and recovery.

```json
{
  "_rift": {
    "flowState": { "backend": "inmemory", "ttlSeconds": 300 }
  },
  "stubs": [{
    "predicates": [{ "equals": { "path": "/api/backend" } }],
    "responses": [{
      "_rift": {
        "script": {
          "engine": "lua",
          "code": "function should_inject(request, flow_store)\n  local fid = 'circuit'\n  local state = flow_store:get(fid, 'state') or 'closed'\n  local failures = flow_store:get(fid, 'failures') or 0\n  local last_failure = flow_store:get(fid, 'last_failure') or 0\n  local now = os.time()\n  \n  -- Half-open: allow one request after 30s recovery\n  if state == 'open' and (now - last_failure) > 30 then\n    flow_store:set(fid, 'state', 'half-open')\n    return { inject = false }  -- Allow probe request\n  end\n  \n  -- Open: reject immediately\n  if state == 'open' then\n    return {\n      inject = true,\n      fault = 'error',\n      status = 503,\n      body = '{\"error\":\"Circuit breaker open\",\"retry_after\":' .. (30 - (now - last_failure)) .. '}',\n      headers = { ['Content-Type'] = 'application/json' }\n    }\n  end\n  \n  -- Simulate 20% backend failure rate\n  if math.random() < 0.2 then\n    failures = failures + 1\n    flow_store:set(fid, 'failures', failures)\n    flow_store:set(fid, 'last_failure', now)\n    \n    -- Trip circuit after 5 failures\n    if failures >= 5 then\n      flow_store:set(fid, 'state', 'open')\n    end\n    \n    return {\n      inject = true,\n      fault = 'error',\n      status = 500,\n      body = '{\"error\":\"Backend failure\",\"failures\":' .. failures .. '}',\n      headers = { ['Content-Type'] = 'application/json' }\n    }\n  end\n  \n  -- Success: reset failures, close circuit\n  flow_store:set(fid, 'failures', 0)\n  flow_store:set(fid, 'state', 'closed')\n  return { inject = false }\nend"
        }
      },
      "is": { "statusCode": 200, "body": "{\"status\": \"ok\"}" }
    }]
  }]
}
```

### When to Use Each Approach

| Scenario | Use Mountebank Cycling | Use Rift Flow State |
|:---------|:----------------------|:--------------------|
| Simple round-robin responses | ✅ | Overkill |
| Test sees responses in order | ✅ | Not needed |
| Multi-user with isolated state | ❌ | ✅ |
| Time-based rate limiting | ❌ | ✅ |
| Complex conditional logic | ❌ | ✅ |
| State survives restart | ❌ | ✅ (Redis) |
| Per-session behavior | ❌ | ✅ |

See [Scripting]({{ site.baseurl }}/features/scripting/) and [Fault Injection]({{ site.baseurl }}/features/fault-injection/) for more examples.

---

## Proxy Responses

Forward requests to real servers and optionally record for later playback.

### Proxy Modes

**proxyAlways** - Always forward, record each response:
```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyAlways"
  }
}
```

**proxyOnce** - Forward first request, replay recorded response:
```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyOnce"
  }
}
```

**proxyTransparent** - Forward without recording:
```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyTransparent"
  }
}
```

### Predicate Generators

Control how recorded stubs are created:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "predicateGenerators": [{
      "matches": {
        "path": true,
        "method": true,
        "query": true
      }
    }]
  }
}
```

### Adding Behaviors to Proxied Responses

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "addDecorateBehavior": "function(request, response) { response.headers['X-Proxied'] = 'true'; return response; }"
  }
}
```

---

## Injection Responses

Generate dynamic responses using JavaScript:

```json
{
  "inject": "function(request, state, logger) { \
    var userId = request.path.split('/')[2]; \
    return { \
      statusCode: 200, \
      headers: { 'Content-Type': 'application/json' }, \
      body: JSON.stringify({ id: userId, name: 'User ' + userId }) \
    }; \
  }"
}
```

### Request Object

Available properties in injection function:

```javascript
request.method    // "GET", "POST", etc.
request.path      // "/api/users/123"
request.query     // { page: "1" }
request.headers   // { "content-type": "application/json" }
request.body      // Request body (string or parsed JSON)
```

### State Object

Persist data across requests:

```javascript
function(request, state, logger) {
  // Initialize counter
  state.counter = state.counter || 0;
  state.counter++;

  return {
    statusCode: 200,
    body: { count: state.counter }
  };
}
```

### Logger Object

Write to Rift logs:

```javascript
function(request, state, logger) {
  logger.info("Processing request to " + request.path);
  return { statusCode: 200 };
}
```

---

## Response Templates

Use EJS templates for dynamic content:

```json
{
  "is": {
    "statusCode": 200,
    "headers": { "Content-Type": "application/json" },
    "body": "{ \"path\": \"<%- request.path %>\", \"timestamp\": \"<%- new Date().toISOString() %>\" }"
  },
  "_behaviors": {
    "decorate": "function(request, response) { return response; }"
  }
}
```

---

## Error Responses

### Client Errors (4xx)

```json
{
  "is": {
    "statusCode": 400,
    "body": { "error": "Bad Request", "message": "Invalid input" }
  }
}

{
  "is": {
    "statusCode": 401,
    "headers": { "WWW-Authenticate": "Bearer" },
    "body": { "error": "Unauthorized" }
  }
}

{
  "is": {
    "statusCode": 404,
    "body": { "error": "Not Found" }
  }
}
```

### Server Errors (5xx)

```json
{
  "is": {
    "statusCode": 500,
    "body": { "error": "Internal Server Error" }
  }
}

{
  "is": {
    "statusCode": 503,
    "headers": { "Retry-After": "60" },
    "body": { "error": "Service Unavailable" }
  }
}
```

---

## Alternative Formats

Rift supports several alternative response formats for compatibility with various tools that generate Mountebank configurations.

### proxy: null with is Response

Some tools include `"proxy": null` alongside an `is` response. This is accepted and the null proxy is ignored:

```json
{
  "responses": [{
    "is": {
      "statusCode": 200,
      "body": "Hello"
    },
    "proxy": null
  }]
}
```

### Combined Alternative Format

A complete example using multiple alternative formats:

```json
{
  "responses": [{
    "behaviors": [{ "wait": 100 }],
    "is": {
      "statusCode": "201",
      "headers": { "Content-Type": "application/json" },
      "body": "{\"created\": true}"
    },
    "proxy": null
  }]
}
```

This example shows:
- `behaviors` without underscore prefix
- `behaviors` as an array
- `statusCode` as a string
- `proxy: null` alongside `is`

---

## Best Practices

1. **Set Content-Type** - Always include appropriate Content-Type header
2. **Use JSON for APIs** - Return `body` as object for automatic serialization
3. **Include error details** - Meaningful error responses help debugging
4. **Use proxy for recording** - Record real API responses for reliable mocks
5. **Keep injection simple** - Complex logic is harder to maintain
