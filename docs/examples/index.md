---
layout: default
title: Examples
nav_order: 9
permalink: /examples/
---

# Configuration Examples

Ready-to-use examples for common use cases.

> **Runnable demos.** Beyond the snippets below, the repository ships a set of `docker-compose`
> demos — Mountebank mode, HTTPS/TLS, fault injection, scripting engines, and retry-proxy — each with
> a working imposter config you can bring up in one command. See
> [`docs/demo/`](https://github.com/achird-labs/rift/tree/master/docs/demo) (start with its
> `README.md`).

---

## REST API Mock

Mock a typical REST API with CRUD operations:

```json
{
  "port": 4545,
  "protocol": "http",
  "name": "User API",
  "stubs": [
    {
      "predicates": [{ "equals": { "method": "GET", "path": "/api/users" } }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "headers": { "Content-Type": "application/json" },
          "body": [
            { "id": 1, "name": "Alice", "email": "alice@example.com" },
            { "id": 2, "name": "Bob", "email": "bob@example.com" }
          ]
        }
      }]
    },
    {
      "predicates": [{
        "and": [
          { "equals": { "method": "GET" } },
          { "matches": { "path": "/api/users/\\d+" } }
        ]
      }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "headers": { "Content-Type": "application/json" },
          "body": { "id": "${id}", "name": "User ${id}" }
        },
        "_behaviors": {
          "copy": {
            "from": { "path": "/api/users/(\\d+)" },
            "into": "${id}",
            "using": { "method": "regex", "selector": "$1" }
          }
        }
      }]
    },
    {
      "predicates": [{ "equals": { "method": "POST", "path": "/api/users" } }],
      "responses": [{
        "is": {
          "statusCode": 201,
          "headers": { "Content-Type": "application/json" },
          "body": { "id": 999, "message": "User created" }
        }
      }]
    },
    {
      "predicates": [{
        "and": [
          { "equals": { "method": "DELETE" } },
          { "matches": { "path": "/api/users/\\d+" } }
        ]
      }],
      "responses": [{ "is": { "statusCode": 204 } }]
    }
  ]
}
```

---

## Authentication Service

Mock OAuth/JWT authentication:

```json
{
  "port": 4546,
  "protocol": "http",
  "name": "Auth Service",
  "stubs": [
    {
      "predicates": [{
        "equals": {
          "method": "POST",
          "path": "/oauth/token",
          "body": { "grant_type": "client_credentials" }
        }
      }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "headers": { "Content-Type": "application/json" },
          "body": {
            "access_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
            "token_type": "Bearer",
            "expires_in": 3600
          }
        }
      }]
    },
    {
      "predicates": [{
        "and": [
          { "equals": { "path": "/api/protected" } },
          { "exists": { "headers": { "Authorization": true } } }
        ]
      }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "body": { "message": "Authenticated!" }
        }
      }]
    },
    {
      "predicates": [{
        "and": [
          { "equals": { "path": "/api/protected" } },
          { "exists": { "headers": { "Authorization": false } } }
        ]
      }],
      "responses": [{
        "is": {
          "statusCode": 401,
          "headers": { "WWW-Authenticate": "Bearer" },
          "body": { "error": "unauthorized" }
        }
      }]
    }
  ]
}
```

---

## Webhook Receiver

Mock a webhook endpoint for testing:

```json
{
  "port": 4547,
  "protocol": "http",
  "name": "Webhook Receiver",
  "recordRequests": true,
  "stubs": [
    {
      "predicates": [{
        "and": [
          { "equals": { "method": "POST", "path": "/webhooks/payment" } },
          { "jsonpath": { "selector": "$.event", "equals": "payment.completed" } }
        ]
      }],
      "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
    },
    {
      "predicates": [{
        "and": [
          { "equals": { "method": "POST", "path": "/webhooks/payment" } },
          { "jsonpath": { "selector": "$.event", "equals": "payment.failed" } }
        ]
      }],
      "responses": [{ "is": { "statusCode": 200, "body": "Acknowledged" } }]
    }
  ]
}
```

---

## Service with Latency

Simulate slow service for timeout testing:

```json
{
  "port": 4548,
  "protocol": "http",
  "name": "Slow Service",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/fast" } }],
      "responses": [{
        "is": { "statusCode": 200, "body": "Fast response" }
      }]
    },
    {
      "predicates": [{ "equals": { "path": "/slow" } }],
      "responses": [{
        "is": { "statusCode": 200, "body": "Slow response" },
        "_behaviors": { "wait": 3000 }
      }]
    },
    {
      "predicates": [{ "equals": { "path": "/random-latency" } }],
      "responses": [{
        "is": { "statusCode": 200, "body": "Variable latency" },
        "_behaviors": {
          "wait": {
            "inject": "function() { return Math.floor(Math.random() * 2000) + 500; }"
          }
        }
      }]
    }
  ]
}
```

---

## Error Simulation

Test error handling in your application:

```json
{
  "port": 4549,
  "protocol": "http",
  "name": "Error Service",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/error/400" } }],
      "responses": [{
        "is": {
          "statusCode": 400,
          "body": { "error": "Bad Request", "code": "INVALID_INPUT" }
        }
      }]
    },
    {
      "predicates": [{ "equals": { "path": "/error/401" } }],
      "responses": [{
        "is": {
          "statusCode": 401,
          "headers": { "WWW-Authenticate": "Bearer realm=\"api\"" },
          "body": { "error": "Unauthorized" }
        }
      }]
    },
    {
      "predicates": [{ "equals": { "path": "/error/429" } }],
      "responses": [{
        "is": {
          "statusCode": 429,
          "headers": { "Retry-After": "60" },
          "body": { "error": "Too Many Requests" }
        }
      }]
    },
    {
      "predicates": [{ "equals": { "path": "/error/500" } }],
      "responses": [{
        "is": {
          "statusCode": 500,
          "body": { "error": "Internal Server Error" }
        }
      }]
    },
    {
      "predicates": [{ "equals": { "path": "/error/503" } }],
      "responses": [{
        "is": {
          "statusCode": 503,
          "headers": { "Retry-After": "30" },
          "body": { "error": "Service Unavailable" }
        }
      }]
    }
  ]
}
```

---

## Proxy with Recording

Record real API responses for offline testing:

```json
{
  "port": 4550,
  "protocol": "http",
  "name": "API Proxy",
  "stubs": [
    {
      "responses": [{
        "proxy": {
          "to": "https://api.example.com",
          "mode": "proxyOnce",
          "predicateGenerators": [{
            "matches": {
              "method": true,
              "path": true,
              "query": true
            }
          }],
          "addDecorateBehavior": "function(request, response) { response.headers['X-Recorded'] = 'true'; return response; }"
        }
      }]
    }
  ]
}
```

---

## HTTPS Imposter

Secure mock server:

```json
{
  "port": 4551,
  "protocol": "https",
  "name": "Secure API",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/secure" } }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "body": { "secure": true }
        }
      }]
    }
  ]
}
```

With custom certificate:

```json
{
  "port": 4551,
  "protocol": "https",
  "key": "-----BEGIN RSA PRIVATE KEY-----\n...\n-----END RSA PRIVATE KEY-----",
  "cert": "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----",
  "stubs": [...]
}
```

Requiring and validating a client certificate (mutual TLS) against your own CA:

```json
{
  "port": 4551,
  "protocol": "https",
  "mutualAuth": true,
  "rejectUnauthorized": true,
  "ca": "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----",
  "stubs": [...]
}
```

A client that presents no certificate, or one that does not chain to `ca`, fails the handshake. See
[TLS/HTTPS]({{ site.baseurl }}/features/tls/#mutual-tls-mtls) for the other combinations and a
walkthrough with `curl`.

---

## Complete Multi-Service Setup

```json
{
  "imposters": [
    {
      "port": 4545,
      "protocol": "http",
      "name": "User Service",
      "stubs": [
        {
          "predicates": [{ "equals": { "path": "/health" } }],
          "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
        },
        {
          "predicates": [{ "equals": { "path": "/users" } }],
          "responses": [{
            "is": {
              "statusCode": 200,
              "body": [{ "id": 1, "name": "Test User" }]
            }
          }]
        }
      ]
    },
    {
      "port": 4546,
      "protocol": "http",
      "name": "Order Service",
      "stubs": [
        {
          "predicates": [{ "equals": { "path": "/health" } }],
          "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
        },
        {
          "predicates": [{ "equals": { "path": "/orders" } }],
          "responses": [{
            "is": {
              "statusCode": 200,
              "body": [{ "id": "ORD-001", "status": "pending" }]
            }
          }]
        }
      ]
    },
    {
      "port": 4547,
      "protocol": "http",
      "name": "Payment Service",
      "stubs": [
        {
          "predicates": [{ "equals": { "path": "/health" } }],
          "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
        },
        {
          "predicates": [{
            "equals": { "method": "POST", "path": "/payments" }
          }],
          "responses": [{
            "is": {
              "statusCode": 201,
              "body": { "id": "PAY-001", "status": "completed" }
            },
            "_behaviors": { "wait": 500 }
          }]
        }
      ]
    }
  ]
}
```
