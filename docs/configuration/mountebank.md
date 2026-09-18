---
layout: default
title: Mountebank Format
parent: Configuration
nav_order: 1
---

# Mountebank Configuration Format

The Mountebank JSON format is the recommended way to configure Rift for service virtualization and API mocking.

---

## Configuration File Structure

```json
{
  "imposters": [
    {
      "port": 4545,
      "protocol": "http",
      "name": "Service Name",
      "stubs": [...],
      "defaultResponse": {...}
    }
  ]
}
```

---

## Imposter Configuration

### Core Fields

No field is strictly required.

| Field | Type | Default | Description |
|:------|:-----|:--------|:------------|
| `port` | number | auto-assigned | Port to listen on. Omitted or `0` means "pick a free port"; imposters that name a port are created first. A `--datadir` file must name one |
| `protocol` | string | `http` | `http` or `https`; anything else (`tcp`, `smtp`) is rejected |
| `host` | string | `0.0.0.0` | Address to bind the imposter to |

### Optional Fields

| Field | Type | Description |
|:------|:-----|:------------|
| `name` | string | Human-readable identifier |
| `stubs` | array | Request/response mappings |
| `defaultResponse` | object | Response when no stub matches |
| `recordRequests` | boolean | Store requests for verification |
| `recordMatches` | boolean | **Accepted, no effect**: per-stub `matches` are not recorded. `true` is reported as `config_key_ignored` in `_rift.warnings` and by `rift-lint` `W017`; use `recordRequests` |
| `allowCORS` | boolean | Add CORS headers to responses |
| `key` | string | PEM private key (HTTPS) |
| `cert` | string | PEM certificate (HTTPS) |
| `mutualAuth` | boolean | Request and **require** a client certificate (`https` only) |
| `rejectUnauthorized` | boolean | Validate the client certificate against `ca`; requires `ca` |
| `ca` | string or array | PEM trust anchor(s) client certificates must chain to |
| `defaultForward` | string | *Rift extension.* Upstream URL an unmatched request is forwarded to (takes precedence over `defaultResponse`) |
| `strictBehaviors` | boolean | *Rift extension.* Turn a failing behavior into a `500` — see [Strict Behaviors]({{ site.baseurl }}/configuration/native/#strict-behaviors-strictbehaviors) |
| `enabled` | boolean | Whether the imposter serves traffic (default `true`); toggled by `POST /imposters/{port}/disable` and `/enable` |

A request that matches no stub gets the `defaultResponse` when there is one, and otherwise a `200`
with an empty body — as in Mountebank, never a `404`.

TLS fields (`key`, `cert`, `mutualAuth`, `rejectUnauthorized`, `ca`) are covered in
[TLS/HTTPS]({{ site.baseurl }}/features/tls/).

### Rift-Specific Metadata Fields

| Field | Type | Description |
|:------|:-----|:------------|
| `serviceName` | string | Service name for documentation (alias: `service_name`) |
| `serviceInfo` | object | Arbitrary metadata (JSON object) |
| `_rift` | object | Rift extensions (flow state, faults, scripting) |

**Example with metadata:**

```json
{
  "port": 4545,
  "protocol": "http",
  "name": "User Service",
  "serviceName": "user-api",
  "serviceInfo": {
    "team": "platform",
    "version": "1.2.3",
    "documentation": "https://docs.example.com/user-api"
  },
  "stubs": [...]
}
```

---

## Stub Configuration

```json
{
  "stubs": [
    {
      "predicates": [...],
      "responses": [...]
    }
  ]
}
```

### Predicates Array

Each predicate object can contain:

```json
{
  "predicates": [
    {
      "equals": { "method": "GET", "path": "/api" },
      "caseSensitive": false,
      "except": ""
    }
  ]
}
```

### Responses Array

```json
{
  "responses": [
    {
      "is": {
        "statusCode": 200,
        "headers": {},
        "body": ""
      },
      "_behaviors": {}
    }
  ]
}
```

---

## Predicate Types Reference

### equals

```json
{ "equals": { "method": "GET", "path": "/users", "query": { "id": "1" } } }
```

### deepEquals

```json
{ "deepEquals": { "body": { "exact": "match" } } }
```

### contains

```json
{ "contains": { "body": "substring" } }
```

### startsWith

```json
{ "startsWith": { "path": "/api" } }
```

### endsWith

```json
{ "endsWith": { "path": ".json" } }
```

### matches

```json
{ "matches": { "path": "/users/\\d+" } }
```

### exists

```json
{ "exists": { "headers": { "Authorization": true } } }
```

### jsonpath

`jsonpath` is a parameter of another predicate, not a predicate of its own: the selector narrows
the body, and the operator beside it tests what was selected.

```json
{ "equals": { "body": 1 }, "jsonpath": { "selector": "$.user.id" } }
```

### xpath

```json
{ "equals": { "body": "1" }, "xpath": { "selector": "//user/id" } }
```

### Logical Operators

```json
{ "and": [ { "equals": {...} }, { "contains": {...} } ] }
{ "or": [ { "equals": {...} }, { "equals": {...} } ] }
{ "not": { "equals": {...} } }
```

---

## Response Types Reference

### is (Static)

```json
{
  "is": {
    "statusCode": 200,
    "headers": { "Content-Type": "application/json" },
    "body": { "key": "value" }
  }
}
```

### proxy

```json
{
  "proxy": {
    "to": "https://api.example.com",
    "mode": "proxyOnce",
    "predicateGenerators": [{ "matches": { "path": true } }]
  }
}
```

### inject

```json
{
  "inject": "function(request, state, logger) { return { statusCode: 200, body: 'Hello' }; }"
}
```

---

## Behaviors Reference

### wait

```json
{ "_behaviors": { "wait": 1000 } }
```

### decorate

```json
{ "_behaviors": { "decorate": "function(request, response) { return response; }" } }
```

### copy

`from` is `"path"`, `"method"` or `"body"`, or `{"query": "<name>"}` / `{"headers": "<name>"}`.
A `regex` selector yields its first capture group (or the whole match when it has none); `jsonpath`
and `xpath` selectors apply to the value `from` picked.

```json
{
  "_behaviors": {
    "copy": {
      "from": "path",
      "into": "${id}",
      "using": { "method": "regex", "selector": "/users/(\\d+)" }
    }
  }
}
```

### lookup

```json
{
  "_behaviors": {
    "lookup": {
      "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
      "fromDataSource": { "csv": { "path": "data.csv", "keyColumn": "id" } },
      "into": "${row}"
    }
  }
}
```

`copy` and `lookup` each accept a single object or an array of them. See
[Behaviors]({{ site.baseurl }}/mountebank/behaviors/) for `shellTransform`, `repeat` and the
error semantics.

Behaviors run on `is` and `inject` responses, and `repeat` on every response. A block setting
anything else on a `proxy`, `fault` or `_rift`-only response is kept and returned, and reported as
`config_key_ignored` in `_rift.warnings` and by `rift-lint` `W017` — see
[Behaviors]({{ site.baseurl }}/mountebank/behaviors/).

---

## HTTPS Configuration

```json
{
  "port": 4545,
  "protocol": "https",
  "key": "-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n-----END RSA PRIVATE KEY-----",
  "cert": "-----BEGIN CERTIFICATE-----\nMIID...\n-----END CERTIFICATE-----",
  "mutualAuth": false,
  "stubs": [...]
}
```

### Using File Paths

```json
{
  "port": 4545,
  "protocol": "https",
  "key": "<%- stringify('/path/to/key.pem') %>",
  "cert": "<%- stringify('/path/to/cert.pem') %>",
  "stubs": [...]
}
```

---

## Complete Example

```json
{
  "imposters": [
    {
      "port": 4545,
      "protocol": "http",
      "name": "User Service",
      "recordRequests": true,
      "defaultResponse": {
        "statusCode": 404,
        "body": { "error": "Not Found" }
      },
      "stubs": [
        {
          "predicates": [
            { "equals": { "method": "GET", "path": "/health" } }
          ],
          "responses": [
            { "is": { "statusCode": 200, "body": "OK" } }
          ]
        },
        {
          "predicates": [
            {
              "and": [
                { "equals": { "method": "GET" } },
                { "matches": { "path": "/users/\\d+" } }
              ]
            }
          ],
          "responses": [
            {
              "is": {
                "statusCode": 200,
                "headers": { "Content-Type": "application/json" },
                "body": { "id": "${id}", "name": "User" }
              },
              "_behaviors": {
                "copy": {
                  "from": "path",
                  "into": "${id}",
                  "using": { "method": "regex", "selector": "/users/(\\d+)" }
                }
              }
            }
          ]
        },
        {
          "predicates": [
            { "equals": { "method": "POST", "path": "/users" } },
            { "exists": { "body": true }, "jsonpath": { "selector": "$.name" } }
          ],
          "responses": [
            {
              "is": {
                "statusCode": 201,
                "body": { "id": 999, "message": "Created" }
              },
              "_behaviors": { "wait": 100 }
            }
          ]
        }
      ]
    }
  ]
}
```

---

## Loading Configuration

### From File at Startup

```bash
# Docker
docker run -p 2525:2525 -p 4545:4545 -v $(pwd)/imposters.json:/imposters.json \
  zainalpour/rift-proxy:latest --configfile /imposters.json

# Binary
rift --configfile imposters.json
```

The file may also be YAML (a sequence of imposters) or a single imposter object, and may use the
Mountebank EJS tags — see [Document shapes and formats]({{ site.baseurl }}/configuration/#document-shapes-and-formats).

### Via REST API

```bash
# Create single imposter
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @imposter.json

# Replace the whole set (imposters absent from the payload are deleted)
curl -X PUT http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @imposters.json
```
