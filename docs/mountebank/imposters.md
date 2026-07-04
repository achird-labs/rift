---
layout: default
title: Imposters
parent: Mountebank Compatibility
nav_order: 1
---

# Imposters

An imposter is a mock server that listens on a specific port and responds to requests based on configured stubs.

---

## Creating an Imposter

### Basic HTTP Imposter

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "name": "My Service Mock",
    "stubs": [{
      "responses": [{
        "is": { "statusCode": 200, "body": "Hello" }
      }]
    }]
  }'
```

### HTTPS Imposter

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4546,
    "protocol": "https",
    "name": "Secure Service Mock",
    "key": "-----BEGIN RSA PRIVATE KEY-----\n...\n-----END RSA PRIVATE KEY-----",
    "cert": "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----",
    "stubs": [{
      "responses": [{
        "is": { "statusCode": 200, "body": "Secure Hello" }
      }]
    }]
  }'
```

---

## Imposter Configuration

| Field | Type | Required | Description |
|:------|:-----|:---------|:------------|
| `port` | number | No | Port to listen on (auto-assigned if omitted) |
| `protocol` | string | No | `http` or `https` (default: `http`) |
| `name` | string | No | Human-readable name |
| `stubs` | array | No | Request/response mappings |
| `defaultResponse` | object | No | Response when no stub matches |
| `recordRequests` | boolean | No | Store requests for verification |
| `allowCORS` | boolean | No | Enable CORS headers and handle preflight requests |
| `service_name` | string | No | Service identifier for documentation |
| `service_info` | object | No | Additional service metadata |
| `key` | string | HTTPS only | PEM-encoded private key |
| `cert` | string | HTTPS only | PEM-encoded certificate |
| `mutualAuth` | boolean | No | Require client certificate |

### HTTP/2 and h2c

Rift auto-negotiates the HTTP version — you don't configure it per imposter:

- **HTTPS imposters** advertise `h2` and `http/1.1` via TLS ALPN, so an HTTP/2-capable client gets
  HTTP/2 and everything else falls back to HTTP/1.1.
- **Plain HTTP imposters** accept **h2c** (cleartext HTTP/2 via prior-knowledge) alongside HTTP/1 —
  the listener detects the HTTP/2 preface and upgrades automatically.

This is on by default and backward-compatible with HTTP/1 clients. Two things force HTTP/1-only:

- an imposter that uses any **TCP fault** (`_rift.fault.tcp` or a top-level `fault`), because a
  connection-level abort is incompatible with HTTP/2 multiplexing; and
- setting the **`RIFT_DISABLE_HTTP2`** environment variable (truthy: `1`/`true`/`yes`/`on`), which
  forces every listener — HTTP and HTTPS, imposter, admin, and metrics — down to HTTP/1.

### Auto-Port Assignment

If you omit the `port` field, Rift will automatically assign an available port from the dynamic range (49152-65535):

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "protocol": "http",
    "stubs": [{
      "responses": [{ "is": { "statusCode": 200 } }]
    }]
  }'

# Response includes the assigned port:
{
  "port": 49152,
  "protocol": "http",
  ...
}
```

---

## Stubs

Each stub contains predicates (matching rules) and responses:

```json
{
  "stubs": [
    {
      "predicates": [
        { "equals": { "method": "GET", "path": "/api/users" } }
      ],
      "responses": [
        { "is": { "statusCode": 200, "body": "[]" } }
      ]
    }
  ]
}
```

### Stub Configuration

| Field | Type | Required | Description |
|:------|:-----|:---------|:------------|
| `id` | string | No | Unique identifier (Rift extension) |
| `predicates` | array | No | Conditions to match requests |
| `responses` | array | Yes | Responses to return |
| `scenarioName` | string | No | Identifier for test scenarios |

The `id` field is a **Rift extension** that allows you to identify stubs by name rather than index:

```json
{
  "stubs": [{
    "id": "get-user-success",
    "predicates": [{ "equals": { "path": "/users/123" } }],
    "responses": [{ "is": { "statusCode": 200, "body": "{\"id\": 123}" } }]
  }]
}
```

The `scenarioName` field is useful for organizing stubs into logical groups for testing:

```json
{
  "stubs": [{
    "scenarioName": "UserService-GetUser-Success",
    "predicates": [{ "equals": { "path": "/users/123" } }],
    "responses": [{ "is": { "statusCode": 200, "body": "{\"id\": 123}" } }]
  }]
}
```

### Multiple Responses (Round-Robin)

When a stub has multiple responses, they cycle through:

```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/flip" } }],
    "responses": [
      { "is": { "body": "heads" } },
      { "is": { "body": "tails" } }
    ]
  }]
}
```

First request returns "heads", second returns "tails", third returns "heads", etc.

---

## Default Response

Configure a fallback response when no stub matches:

```json
{
  "port": 4545,
  "protocol": "http",
  "defaultResponse": {
    "statusCode": 404,
    "headers": { "Content-Type": "application/json" },
    "body": { "error": "Not Found" }
  },
  "stubs": [...]
}
```

---

## Recording Requests

Enable request recording for verification in tests:

```json
{
  "port": 4545,
  "protocol": "http",
  "recordRequests": true,
  "stubs": [...]
}
```

Retrieve recorded requests:

```bash
curl http://localhost:2525/imposters/4545

# Response includes:
{
  "requests": [
    {
      "method": "GET",
      "path": "/api/users",
      "headers": {...},
      "body": "",
      "timestamp": "2024-01-15T10:30:00.000Z"
    }
  ]
}
```

---

## Managing Imposters

### List All Imposters

```bash
curl http://localhost:2525/imposters

# Response:
{
  "imposters": [
    { "port": 4545, "protocol": "http", "name": "User Service" },
    { "port": 4546, "protocol": "https", "name": "Payment Service" }
  ]
}
```

### Get Imposter Details

```bash
curl http://localhost:2525/imposters/4545

# Response includes full configuration and recorded requests
```

### Delete Single Imposter

```bash
curl -X DELETE http://localhost:2525/imposters/4545
```

### Delete All Imposters

```bash
curl -X DELETE http://localhost:2525/imposters
```

---

## Loading from Configuration File

### JSON Format

Create `imposters.json`:

```json
{
  "imposters": [
    {
      "port": 4545,
      "protocol": "http",
      "stubs": [...]
    },
    {
      "port": 4546,
      "protocol": "http",
      "stubs": [...]
    }
  ]
}
```

Load on startup:

```bash
docker run -v $(pwd)/imposters.json:/imposters.json \
  zainalpour/rift-proxy:latest --configfile /imposters.json
```

### EJS Templates

Use EJS for dynamic configuration:

```json
{
  "imposters": [
    {
      "port": "<%= port || 4545 %>",
      "protocol": "http",
      "stubs": [...]
    }
  ]
}
```

---

## Stub Matching Behavior

### First-Match-Wins

Stubs are evaluated in order. The **first stub whose predicates match** is used:

```json
{
  "stubs": [
    {
      "predicates": [{ "startsWith": { "path": "/api" } }],
      "responses": [{ "is": { "body": "general" } }]
    },
    {
      "predicates": [{ "equals": { "path": "/api/users" } }],
      "responses": [{ "is": { "body": "specific" } }]
    }
  ]
}
```

In this example, `/api/users` returns "general" because the first stub matches. To get "specific", swap the stub order.

### Empty Predicates (Catch-All)

A stub with empty predicates matches **all requests**:

```json
{
  "stubs": [
    { "predicates": [], "responses": [{ "is": { "body": "catch all" } }] }
  ]
}
```

**Warning**: A catch-all stub shadows all subsequent stubs. Place catch-all stubs last.

### Index-Based Operations

Stub indexes shift when stubs are added or removed:

```
Before: [Stub0, Stub1, Stub2]  (indexes 0, 1, 2)
Delete index 0:
After:  [Stub1, Stub2]         (indexes 0, 1)
```

---

## Rift Stub Analysis (Rift Extension)

Rift provides optional warnings for common stub configuration issues. These warnings appear in the API response under `_rift.warnings`:

```bash
curl http://localhost:2525/imposters/4545

# Response includes:
{
  "port": 4545,
  "stubs": [...],
  "_rift": {
    "warnings": [
      {
        "warningType": "catch_all_not_last",
        "message": "Catch-all stub at index 0 will shadow 2 stub(s) after it",
        "stubIndex": 0
      }
    ]
  }
}
```

### Warning Types

| Type | Description |
|:-----|:------------|
| `duplicate_id` | Multiple stubs have the same ID |
| `exact_duplicate` | Stub predicates are identical to another stub |
| `potentially_shadowed` | Stub may be unreachable due to earlier stub |
| `catch_all` | Stub with empty predicates matches all requests |
| `catch_all_not_last` | Catch-all stub is not at the end of the list |

**Note**: Mountebank does NOT provide overlap detection. These warnings are a Rift extension.

---

## Best Practices

1. **Use meaningful names** - Makes debugging easier
2. **Order stubs specifically** - More specific predicates first
3. **Enable recording in tests** - Verify expected requests
4. **Use default responses** - Clear error messages for unmatched requests
5. **Separate imposters by service** - One imposter per external dependency
6. **Place catch-all stubs last** - Avoid accidentally shadowing specific stubs
7. **Use stub IDs** (Rift) - Easier to track and manage stubs by name
