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

### Required Fields

| Field | Type | Description |
|:------|:-----|:------------|
| `port` | number | Port to listen on |
| `protocol` | string | `http` or `https` |

### Optional Fields

| Field | Type | Description |
|:------|:-----|:------------|
| `name` | string | Human-readable identifier |
| `stubs` | array | Request/response mappings |
| `defaultResponse` | object | Response when no stub matches |
| `recordRequests` | boolean | Store requests for verification |
| `recordMatches` | boolean | Record which stub matched each request |
| `allowCORS` | boolean | Add CORS headers to responses |
| `key` | string | PEM private key (HTTPS) |
| `cert` | string | PEM certificate (HTTPS) |
| `mutualAuth` | boolean | Request and **require** a client certificate (`https` only) |
| `rejectUnauthorized` | boolean | Validate the client certificate against `ca`; requires `ca` |
| `ca` | string or array | PEM trust anchor(s) client certificates must chain to |

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

```json
{ "jsonpath": { "selector": "$.user.id", "equals": 1 } }
```

### xpath

```json
{ "xpath": { "selector": "//user/id", "equals": "1" } }
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

```json
{
  "_behaviors": {
    "copy": {
      "from": { "path": "/(\\d+)" },
      "into": "${id}",
      "using": { "method": "regex", "selector": "$1" }
    }
  }
}
```

### lookup

```json
{
  "_behaviors": {
    "lookup": {
      "key": { "from": "query", "using": { "method": "jsonpath", "selector": "$.id" } },
      "fromDataSource": { "csv": { "path": "data.csv", "keyColumn": "id" } },
      "into": "${row}"
    }
  }
}
```

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
  "key": "<%- include('/path/to/key.pem') %>",
  "cert": "<%- include('/path/to/cert.pem') %>",
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
                  "from": { "path": "/users/(\\d+)" },
                  "into": "${id}",
                  "using": { "method": "regex", "selector": "$1" }
                }
              }
            }
          ]
        },
        {
          "predicates": [
            { "equals": { "method": "POST", "path": "/users" } },
            { "jsonpath": { "selector": "$.name", "exists": true } }
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
docker run -v $(pwd)/imposters.json:/imposters.json \
  zainalpour/rift-proxy:latest --configfile /imposters.json

# Binary
./rift --configfile imposters.json
```

### Via REST API

```bash
# Create single imposter
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @imposter.json

# Load multiple imposters
curl -X PUT http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @imposters.json
```
