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

`decorate`, `shellTransform` and a `wait` written as a JavaScript function run code, so an imposter
using any of them is refused unless the server was started with `--allowInjection`. A numeric
`wait`, `repeat`, `copy` and `lookup` need no flag. A behavior key set to `null` is treated as
absent everywhere, including by that check.

Behaviors apply to `is` responses (and the flat response form). On a `proxy`, `inject` or `fault`
response they are ignored.

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

When using array format, behaviors are merged into a single object. If the same behavior type appears multiple times, the last one replaces the earlier ones — two array elements that each hold a `copy` keep only the second; put both copies in one `copy` array instead. `_behaviors` takes precedence over `behaviors` when both are present; `"_behaviors": null` counts as absent, so `behaviors` is used.

`_behaviors` must be an object; the array form is only accepted under `behaviors`, and each of its elements must be an object (a non-object element is skipped). Any other shape — an array or scalar `_behaviors`, or a scalar `behaviors` — is refused: `POST /imposters` returns `400` and a config file fails to load. This holds with `--allowInjection` on too.

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

A `{min, max}` range picks a delay uniformly between the two, inclusive (Rift extension):

```json
{
  "_behaviors": {
    "wait": { "min": 500, "max": 1500 }
  }
}
```

A JavaScript function can compute the delay instead. The `{"inject": ...}` object spelling is a Rift
extension; Mountebank's own spelling is the bare function string shown in the next section.

```json
{
  "_behaviors": {
    "wait": {
      "inject": "function() { return Math.floor(Math.random() * 1000) + 500; }"
    }
  }
}
```

Returns random delay between 500-1500ms. A delay computed by a function is capped at 60 seconds;
a function that throws or returns no usable number falls back to 100ms and logs a warning.

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

`response.body` arrives as a **string**, even when the stub's `body` is a JSON object, so parse it
before changing fields. If the function sets `response.body` to an object, Rift serializes it back
to JSON. `response.headers` holds one string per header name.

### Basic Transformation

```json
{
  "is": {
    "statusCode": 200,
    "body": { "data": [] }
  },
  "_behaviors": {
    "decorate": "function(request, response) { var body = JSON.parse(response.body); body.timestamp = Date.now(); response.body = body; return response; }"
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

## shellTransform

Pipe the response through one or more external shell commands. Useful for transforming a response
body with an existing script or CLI tool.

`shellTransform` accepts a single command string, or an array of commands that are **chained in
sequence** (each command's output feeds the next):

```json
{
  "is": { "statusCode": 200, "body": "hello" },
  "_behaviors": {
    "shellTransform": "tr a-z A-Z"
  }
}
```

```json
{
  "_behaviors": {
    "shellTransform": ["./add-header.sh", "./rewrite-body.sh"]
  }
}
```

Each command runs via `sh -c "<command>"` and receives two environment variables:

| Variable | JSON shape |
|:---------|:-----------|
| `MB_REQUEST` | `{ "method", "path", "query", "headers", "body" }` |
| `MB_RESPONSE` | `{ "statusCode", "body" }` |

`MB_REQUEST.headers` holds one string per header name: a header the client sent more than once
contributes its **first** value, and a header whose value was not valid UTF-8 is absent from the
object entirely rather than present as `""` (#1040). That is the same view `copy`, `lookup`,
`decorate` and `${request.headers.*}` read, and the same one predicates match on — see
[predicates](predicates.md).

The command's **stdout becomes the new response body**. A non-zero exit is a failure: by default it
is lenient (the original body is served and an `x-rift-shelltransform-error: true` header is added);
with `strictBehaviors` / `RIFT_STRICT_BEHAVIORS` it returns `500` (see
[Error Semantics](#error-semantics)).

---

## copy

Copy values from the request to the response. Useful for echoing request data.

`from` names the request field: `"path"`, `"method"` or `"body"` as a string, or
`{"query": "<name>"}` / `{"headers": "<name>"}` for one query parameter or header. `using` extracts
from that value:

| `method` | `selector` | Result |
|:---------|:-----------|:-------|
| `regex` | A regular expression | The first capture group, or the whole match if the pattern has none. `options` takes `ignoreCase` and `multiline`. |
| `jsonpath` | A JSONPath selector | The selected value from a JSON source |
| `xpath` | An XPath selector | The selected value from an XML source |

Every occurrence of the `into` token in the body and header values is replaced. If the source is
absent, or nothing is extracted, the token is replaced with an empty string.

Rift returns the first capture group where Mountebank returns the whole regex match, so a pattern
like `/users/(\d+)` yields just the id.

When a `copy` token sits in a **header** value, the substituted text comes from the request, so
Rift removes any character a header value cannot carry (CR, LF, NUL and the other ASCII controls)
and logs a `rift::template` warning naming what it removed, with `port`, `stub` and `stub_id` identifying the stub. A tab and any non-ASCII character are
legal and are kept. Only the substituted text is repaired — a control character you wrote literally
into the header is left alone, and still fails that response with a `500`.

### Copy from Path

```json
{
  "is": {
    "statusCode": 200,
    "body": { "id": "${id}" }
  },
  "_behaviors": {
    "copy": {
      "from": "path",
      "into": "${id}",
      "using": { "method": "regex", "selector": "/users/(\\d+)" }
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
      "from": { "query": "page" },
      "into": "${page}",
      "using": { "method": "regex", "selector": ".+" }
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
      "from": { "headers": "X-Request-Id" },
      "into": "${reqId}",
      "using": { "method": "regex", "selector": ".+" }
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
        "from": "path",
        "into": "${orderId}",
        "using": { "method": "regex", "selector": "/orders/(\\d+)" }
      },
      {
        "from": { "query": "format" },
        "into": "${format}",
        "using": { "method": "regex", "selector": ".+" }
      }
    ]
  }
}
```

---

## lookup

Look up a row in a CSV file, keyed by a value extracted from the request. `key` takes the same
`from` and `using` as [`copy`](#copy). Each column of the matched row replaces the token
`<into>[<column>]`, so with `"into": "${row}"` the `email` column fills `${row}[email]`.

As with [`copy`](#copy), a `lookup` token in a **header** value is repaired after substitution: the
request chooses which row is read, so a CSV cell holding a character a header value cannot carry
would otherwise fail the whole response. Those characters are removed from the substituted cell and
a `rift::template` warning names them, alongside the `port`, `stub` and `stub_id` of the stub that produced it. Literal text you wrote around the token is left alone.

### CSV Lookup

```json
{
  "is": {
    "statusCode": 200,
    "body": { "name": "${row}[name]", "email": "${row}[email]" }
  },
  "_behaviors": {
    "lookup": {
      "key": {
        "from": "path",
        "using": { "method": "regex", "selector": "/users/(\\d+)" }
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

`csv.delimiter` sets a single-character separator (default `,`). The file is read once and cached;
its path is resolved relative to the server's working directory. Cells are split on the delimiter
without CSV quoting rules, so a quoted cell containing the delimiter is not supported. If the file
cannot be read, a warning is logged and the tokens are left in place.

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

When multiple behaviors are defined on an `is` response, they execute in this order:

1. **wait** - Delay first
2. **copy** - Copy request values into response
3. **lookup** - Perform data lookups
4. **decorate** - Transform the response
5. **shellTransform** - Pipe the body through each command in turn

`repeat` is not a step here; it controls which response is chosen.

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
      "from": "path",
      "into": "${id}",
      "using": { "method": "regex", "selector": "/users/(\\d+)" }
    },
    "decorate": "function(request, response) { var body = JSON.parse(response.body); body.processed = true; body.timestamp = Date.now(); response.body = body; return response; }",
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
| `binary` (base64) — an `is` body **or** a `defaultResponse` body | `x-rift-binary-error: true` |

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
