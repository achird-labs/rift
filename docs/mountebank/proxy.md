---
layout: default
title: Proxy Mode
parent: Mountebank Compatibility
nav_order: 5
---

# Proxy Mode

Proxy mode forwards requests to real servers and optionally records responses for playback. This is useful for creating mocks from real API behavior.

---

## Basic Proxy

Forward all requests to a backend server:

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [{
    "responses": [{
      "proxy": {
        "to": "https://api.example.com"
      }
    }]
  }]
}
```

---

## Proxy Modes

### proxyAlways

Always forward requests; record a new stub for each unique request:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyAlways"
  }
}
```

Use when: Building a comprehensive mock from varied requests.

### proxyOnce

Forward the first request, then replay the recorded response:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyOnce"
  }
}
```

Use when: Recording a fixed set of responses for offline testing.

### proxyTransparent

Forward without recording (pure reverse proxy):

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyTransparent"
  }
}
```

Use when: Acting as a transparent proxy without mocking.

---

## Predicate Generators

Control how requests are matched when generating stubs:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "predicateGenerators": [{
      "matches": {
        "method": true,
        "path": true,
        "query": true
      }
    }]
  }
}
```

### Available Fields

| Field | Description |
|:------|:------------|
| `method` | Match HTTP method |
| `path` | Match request path |
| `query` | Match query parameters |
| `headers` | Match request headers |
| `body` | Match request body |

### Selective Matching

Match only specific aspects:

```json
{
  "predicateGenerators": [{
    "matches": {
      "method": true,
      "path": true
    }
  }]
}
```

This generates stubs that match method and path, ignoring query and body.

### Case Sensitivity

```json
{
  "predicateGenerators": [{
    "matches": { "path": true },
    "caseSensitive": true
  }]
}
```

### Generation Failures

A predicate generator can also be an [`inject`](../features/scripting.md) function that builds
predicates in JavaScript. If that generation **fails** — the script throws, returns something other
than a predicate array, the script pool is unavailable, or it exceeds the script timeout — Rift does
**not** record a stub. Recording a stub with the empty/partial predicate list would produce a
match-all stub that shadows every future request, so the failure is surfaced instead of hidden:

- **no stub is recorded** for that request (the proxied response is still returned to the client), and
- the proxied response carries an `x-rift-generator-error` header whose value names the failure —
  `script-error`, `invalid-output`, `pool-failure`, `timeout`, or `task-panic`.

A generator that legitimately returns an empty array (`[]`) is **not** a failure — it records a
match-all stub as before. Only genuine generation failures skip recording and set the header.

---

## Recording Workflow

### Step 1: Create Recording Proxy

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "stubs": [{
      "responses": [{
        "proxy": {
          "to": "https://api.example.com",
          "mode": "proxyOnce",
          "predicateGenerators": [{
            "matches": { "method": true, "path": true, "query": true }
          }]
        }
      }]
    }]
  }'
```

### Step 2: Run Your Tests

```bash
# Run application tests against the proxy
npm test
# or
pytest
```

### Step 3: Export Recorded Stubs

```bash
curl http://localhost:2525/imposters/4545?replayable=true > recorded.json
```

### Step 4: Use Recorded Mocks

```bash
# Delete proxy imposter
curl -X DELETE http://localhost:2525/imposters/4545

# Load recorded mocks
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @recorded.json
```

### Flat Response Form

Rift accepts a stub response in **flat form** — `statusCode`, `headers`, and `body` at the top level
of the response object, with no `is` wrapper — and serves it identically to the wrapped form. This
matters when replaying recorded or externally-generated mocks that emit flat responses:

```json
// Flat form (no "is" wrapper) — accepted and served as a 200 with the body
{ "statusCode": 200, "body": "recorded" }

// Equivalent to the canonical wrapped form
{ "is": { "statusCode": 200, "body": "recorded" } }
```

`statusCode` defaults to `200` when omitted. If both a top-level field and an explicit `is` are
present, `is` takes precedence.

---

## Modifying Proxied Responses

### addDecorateBehavior

Transform proxied responses before recording:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "addDecorateBehavior": "function(request, response) { \
      response.headers['X-Proxied-By'] = 'Rift'; \
      return response; \
    }"
  }
}
```

### addWaitBehavior

Add latency to proxied responses:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "addWaitBehavior": 100
  }
}
```

---

## HTTPS Proxy

### Proxy to HTTPS Backend

```json
{
  "proxy": {
    "to": "https://secure-api.example.com"
  }
}
```

### Trusting a Private CA

Rift verifies outbound TLS against the operating system trust store. An origin issued by an
internal CA — a corporate API gateway — needs that CA supplied:

```bash
rift --upstream-ca-file /etc/rift/corp-ca.pem
# or
RIFT_UPSTREAM_CA_FILE=/etc/rift/corp-ca.pem rift
```

The anchor is **appended** to the OS store, so public origins keep working. This applies to every
outbound call Rift makes: `proxy` stub upstreams and `--configfile https://…` alike.

> `SSL_CERT_FILE` / `SSL_CERT_DIR` are also honoured, but they **replace** the trust store rather
> than adding to it — pointing `SSL_CERT_FILE` at a lone private CA silently drops every public
> root. Use `--upstream-ca-file` unless you are supplying a complete bundle.

### Skipping Verification (development only)

```bash
rift --upstream-tls-skip-verify
```

Accepts any certificate and logs a warning. Prefer `--upstream-ca-file`: a recording proxy with
verification disabled will faithfully record MITM'd traffic.

> `key`, `cert` and `ciphers` on a `proxy` response are accepted for Mountebank compatibility and
> **are not honoured** — Rift's outbound trust is process-wide, configured by the two flags above.

---

## Header Manipulation

### Inject Headers

Add headers to proxied requests:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "injectHeaders": {
      "X-Forwarded-By": "Rift",
      "Authorization": "Bearer token123"
    }
  }
}
```

---

## Path Rewriting

Modify the request path before forwarding to the backend:

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "pathRewrite": {
      "from": "/api/v2",
      "to": "/api/v1"
    }
  }
}
```

### Use Cases

**Version Migration:**

Route v2 API calls to v1 backend during migration:

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [{
    "predicates": [{ "startsWith": { "path": "/api/v2" } }],
    "responses": [{
      "proxy": {
        "to": "https://legacy-api.example.com",
        "pathRewrite": {
          "from": "/api/v2",
          "to": "/api/v1"
        }
      }
    }]
  }]
}
```

**Strip Prefix:**

Remove a prefix from paths:

```json
{
  "proxy": {
    "to": "https://backend.internal",
    "pathRewrite": {
      "from": "/gateway/service",
      "to": ""
    }
  }
}
```

Request to `/gateway/service/users` → forwards to `/users`

---

## Combining Proxy with Stubs

Mix static stubs with proxy fallback:

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/mocked" } }],
      "responses": [{
        "is": { "statusCode": 200, "body": "Mocked response" }
      }]
    },
    {
      "responses": [{
        "proxy": {
          "to": "https://api.example.com",
          "mode": "proxyTransparent"
        }
      }]
    }
  ]
}
```

Requests to `/mocked` return the static response; all others proxy to the real API.

---

## Example: API Gateway Pattern

Create a proxy that records and allows overriding specific endpoints:

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/health" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
    },
    {
      "predicates": [{ "equals": { "path": "/api/feature-flag" } }],
      "responses": [{ "is": { "body": { "enabled": true } } }]
    },
    {
      "responses": [{
        "proxy": {
          "to": "https://api.example.com",
          "mode": "proxyOnce",
          "predicateGenerators": [{
            "matches": { "method": true, "path": true }
          }]
        }
      }]
    }
  ]
}
```

---

## Best Practices

1. **Start with proxyOnce** - Record responses for consistent tests
2. **Use predicate generators wisely** - Too specific = too many stubs
3. **Export regularly** - Save recorded mocks to version control
4. **Clean up sensitive data** - Review recorded responses for secrets
5. **Use proxyTransparent for debugging** - See actual API responses
