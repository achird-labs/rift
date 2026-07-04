---
layout: default
title: Behaviors
parent: Mountebank Compatibility
nav_order: 4
---

# Behaviors

Behaviors modify responses before they are sent to the client. They enable latency simulation, response transformation, and dynamic content.

---

## Adding Behaviors

Behaviors are added to responses using `_behaviors`:

```json
{
  "is": {
    "statusCode": 200,
    "body": "Hello"
  },
  "_behaviors": {
    "wait": 1000,
    "decorate": "function(request, response) { response.body += ' World'; return response; }"
  }
}
```

### Alternative Format: behaviors (without underscore)

Some tools generate `behaviors` without the underscore prefix. Both formats are supported:

```json
{
  "is": { "statusCode": 200 },
  "behaviors": {
    "wait": 1000
  }
}
```

### Alternative Format: behaviors as Array

Behaviors can also be specified as an array of behavior objects:

```json
{
  "is": { "statusCode": 200, "body": "Hello" },
  "behaviors": [
    { "wait": 100 },
    { "decorate": "function(request, response) { response.body += ' World'; return response; }" }
  ]
}
```

When using array format, behaviors are merged into a single object. If the same behavior type appears multiple times, the last one takes precedence.

---

## wait

Add latency to responses. Essential for testing timeout handling.

### Fixed Delay

```json
{
  "_behaviors": {
    "wait": 2000
  }
}
```

Adds exactly 2000ms delay.

### Random Delay

```json
{
  "_behaviors": {
    "wait": {
      "inject": "function() { return Math.floor(Math.random() * 1000) + 500; }"
    }
  }
}
```

Returns random delay between 500-1500ms.

### JavaScript Function String

Some tools generate wait as a direct JavaScript function string:

```json
{
  "behaviors": [{
    "wait": " function() { var min = Math.ceil(0); var max = Math.floor(100); return Math.floor(Math.random() * (max - min + 1)) + min; } "
  }]
}
```

This format is supported and the function is evaluated to compute the delay.

### Use Cases

**Test client timeouts:**
```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/slow-endpoint" } }],
    "responses": [{
      "is": { "statusCode": 200 },
      "_behaviors": { "wait": 5000 }
    }]
  }]
}
```

**Simulate network latency:**
```json
{
  "_behaviors": {
    "wait": {
      "inject": "function() { return Math.floor(Math.random() * 100) + 50; }"
    }
  }
}
```

---

## decorate

Transform responses using JavaScript. The function receives request and response, and must return the modified response.

### Basic Transformation

```json
{
  "is": {
    "statusCode": 200,
    "body": { "data": [] }
  },
  "_behaviors": {
    "decorate": "function(request, response) { response.body.timestamp = Date.now(); return response; }"
  }
}
```

### Add Request Info to Response

```json
{
  "_behaviors": {
    "decorate": "function(request, response) { \
      response.headers = response.headers || {}; \
      response.headers['X-Request-Path'] = request.path; \
      response.headers['X-Request-Method'] = request.method; \
      return response; \
    }"
  }
}
```

### Conditional Modification

```json
{
  "_behaviors": {
    "decorate": "function(request, response) { \
      if (request.headers['X-Debug'] === 'true') { \
        response.body = { \
          original: response.body, \
          debug: { path: request.path, query: request.query } \
        }; \
      } \
      return response; \
    }"
  }
}
```

### Parse and Modify JSON

```json
{
  "_behaviors": {
    "decorate": "function(request, response) { \
      var body = typeof response.body === 'string' ? JSON.parse(response.body) : response.body; \
      body.serverTime = new Date().toISOString(); \
      response.body = body; \
      return response; \
    }"
  }
}
```

---

## copy

Copy values from the request to the response. Useful for echoing request data.

### Copy from Path

```json
{
  "is": {
    "statusCode": 200,
    "body": { "id": "${id}" }
  },
  "_behaviors": {
    "copy": {
      "from": { "path": "/users/(\\d+)" },
      "into": "${id}",
      "using": { "method": "regex", "selector": "$1" }
    }
  }
}
```

Request to `/users/123` returns `{ "id": "123" }`.

### Copy from Query

```json
{
  "is": {
    "statusCode": 200,
    "body": "Page: ${page}"
  },
  "_behaviors": {
    "copy": {
      "from": "query",
      "into": "${page}",
      "using": { "method": "jsonpath", "selector": "$.page" }
    }
  }
}
```

### Copy from Headers

```json
{
  "is": {
    "statusCode": 200,
    "headers": { "X-Request-Id": "${reqId}" }
  },
  "_behaviors": {
    "copy": {
      "from": "headers",
      "into": "${reqId}",
      "using": { "method": "jsonpath", "selector": "$['X-Request-Id']" }
    }
  }
}
```

### Copy from Body

```json
{
  "is": {
    "statusCode": 200,
    "body": { "received": "${name}" }
  },
  "_behaviors": {
    "copy": {
      "from": "body",
      "into": "${name}",
      "using": { "method": "jsonpath", "selector": "$.user.name" }
    }
  }
}
```

### Multiple Copies

```json
{
  "_behaviors": {
    "copy": [
      {
        "from": { "path": "/orders/(\\d+)" },
        "into": "${orderId}",
        "using": { "method": "regex", "selector": "$1" }
      },
      {
        "from": "query",
        "into": "${format}",
        "using": { "method": "jsonpath", "selector": "$.format" }
      }
    ]
  }
}
```

---

## lookup

Look up data from external sources (CSV files, etc.).

### CSV Lookup

```json
{
  "is": {
    "statusCode": 200,
    "body": { "name": "${name}", "email": "${email}" }
  },
  "_behaviors": {
    "lookup": {
      "key": {
        "from": { "path": "/users/(\\d+)" },
        "using": { "method": "regex", "selector": "$1" }
      },
      "fromDataSource": {
        "csv": {
          "path": "users.csv",
          "keyColumn": "id"
        }
      },
      "into": "${row}"
    }
  }
}
```

With `users.csv`:
```csv
id,name,email
1,Alice,alice@example.com
2,Bob,bob@example.com
```

Request to `/users/1` returns `{ "name": "Alice", "email": "alice@example.com" }`.

---

## repeat

Control how many times a response is returned before cycling to the next response.

### Basic Repeat

```json
{
  "responses": [
    {
      "is": { "statusCode": 200, "body": "First response" },
      "_behaviors": { "repeat": 3 }
    },
    {
      "is": { "statusCode": 200, "body": "Second response" }
    }
  ]
}
```

The first response is returned 3 times before advancing to the second:
- Requests 1-3 → "First response"
- Request 4 → "Second response"
- Requests 5-7 → "First response" (cycles back)
- Request 8 → "Second response"

### Per-Response Repeat

Each response can have its own repeat count:

```json
{
  "responses": [
    {
      "is": { "statusCode": 200, "body": "Success" },
      "_behaviors": { "repeat": 5 }
    },
    {
      "is": { "statusCode": 500, "body": "Error" },
      "_behaviors": { "repeat": 2 }
    }
  ]
}
```

Returns "Success" 5 times, then "Error" 2 times, then cycles.

### Use Cases

**Simulating rate limiting:**
```json
{
  "responses": [
    {
      "is": { "statusCode": 200, "body": "OK" },
      "_behaviors": { "repeat": 10 }
    },
    {
      "is": {
        "statusCode": 429,
        "headers": { "Retry-After": "60" },
        "body": "Rate limited"
      }
    }
  ]
}
```

Allows 10 requests, then returns 429, then cycles.

**Simulating quota exhaustion:**
```json
{
  "responses": [
    {
      "is": { "body": { "remaining": 100 } },
      "_behaviors": { "repeat": 50 }
    },
    {
      "is": { "body": { "remaining": 50 } },
      "_behaviors": { "repeat": 50 }
    },
    {
      "is": { "statusCode": 403, "body": "Quota exceeded" }
    }
  ]
}
```

**Testing retry logic with eventual success:**
```json
{
  "responses": [
    {
      "is": { "statusCode": 503, "body": "Service unavailable" },
      "_behaviors": { "repeat": 2 }
    },
    {
      "is": { "statusCode": 200, "body": "Success after retries" }
    }
  ]
}
```

Fails twice, then succeeds - perfect for testing retry mechanisms.

### Without Repeat

Responses without `repeat` default to 1 - they are returned once before advancing:

```json
{
  "responses": [
    { "is": { "body": "First" } },
    { "is": { "body": "Second" } },
    { "is": { "body": "Third" } }
  ]
}
```

Each response is returned once in sequence (standard cycling).

---

## Behavior Order

When multiple behaviors are defined, they execute in this order:

1. **copy** - Copy request values into response
2. **lookup** - Perform data lookups
3. **decorate** - Transform the response
4. **wait** - Add delay before sending

---

## Combining Behaviors

```json
{
  "is": {
    "statusCode": 200,
    "body": { "userId": "${id}", "processed": false }
  },
  "_behaviors": {
    "copy": {
      "from": { "path": "/users/(\\d+)" },
      "into": "${id}",
      "using": { "method": "regex", "selector": "$1" }
    },
    "decorate": "function(request, response) { \
      response.body.processed = true; \
      response.body.timestamp = Date.now(); \
      return response; \
    }",
    "wait": 100
  }
}
```

---

## Error Semantics

When a transforming behavior fails — a `decorate` function throws, a `shellTransform` command exits
non-zero, or a `binary`/base64 body can't be decoded — Rift signals the failure rather than failing
silently.

**Default (lenient).** The fallback response (the un-transformed body) is still served with its
normal status, and a header flags what failed:

| Behavior | Failure header |
|:---------|:---------------|
| `decorate` | `x-rift-decorate-error: true` |
| `shellTransform` | `x-rift-shelltransform-error: true` |
| `binary` (base64) | `x-rift-binary-error: true` |

**Strict mode.** Set the per-imposter `strictBehaviors` flag (or the `RIFT_STRICT_BEHAVIORS`
environment variable, truthy: `1`/`true`/`yes`/`on`) to turn a behavior failure into a
`500 Internal Server Error` — the failing behavior no longer serves a fallback. The response still
carries the matching `x-rift-<behavior>-error` header. The per-imposter flag and the env var combine
with **OR**: either being set forces strict mode. The default is lenient (both unset).

```json
{
  "port": 4545,
  "protocol": "http",
  "strictBehaviors": true,
  "stubs": [{
    "responses": [{
      "is": { "statusCode": 200, "body": "Hello" },
      "_behaviors": {
        "decorate": "function(request, response) { throw new Error('boom'); }"
      }
    }]
  }]
}
```

With `strictBehaviors` on, the throwing `decorate` above returns `500` instead of serving `"Hello"`.

---

## Best Practices

1. **Use wait sparingly** - Only for testing timeout handling
2. **Keep decorate functions simple** - Complex logic is hard to debug
3. **Use copy for echoing** - More maintainable than decorate for simple cases
4. **Test behaviors individually** - Easier to debug
5. **Document behavior purpose** - Future maintainers will thank you
