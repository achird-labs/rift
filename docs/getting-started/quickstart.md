---
layout: default
title: Quick Start
parent: Getting Started
nav_order: 1
---

# Quick Start Tutorial

This tutorial walks you through creating various types of imposters with Rift.

---

## Prerequisites

Ensure Rift is running:

```bash
# The admin port plus the imposter ports this tutorial uses (4545-4552)
docker run -p 2525:2525 -p 4545-4552:4545-4552 zainalpour/rift-proxy:latest
```

---

## Basic REST API Mock

Create a mock for a simple user API:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "name": "User API",
    "stubs": [
      {
        "predicates": [{ "equals": { "method": "GET", "path": "/users" } }],
        "responses": [{
          "is": {
            "statusCode": 200,
            "headers": { "Content-Type": "application/json" },
            "body": [
              { "id": 1, "name": "Alice" },
              { "id": 2, "name": "Bob" }
            ]
          }
        }]
      },
      {
        "predicates": [{ "equals": { "method": "GET", "path": "/users/1" } }],
        "responses": [{
          "is": {
            "statusCode": 200,
            "headers": { "Content-Type": "application/json" },
            "body": { "id": 1, "name": "Alice", "email": "alice@example.com" }
          }
        }]
      },
      {
        "predicates": [{ "equals": { "method": "POST", "path": "/users" } }],
        "responses": [{
          "is": {
            "statusCode": 201,
            "headers": { "Content-Type": "application/json" },
            "body": { "id": 3, "name": "New User" }
          }
        }]
      }
    ]
  }'
```

Test the endpoints:

```bash
# List users
curl http://localhost:4545/users
# [{"id":1,"name":"Alice"},{"id":2,"name":"Bob"}]

# Get user by ID
curl http://localhost:4545/users/1
# {"id":1,"name":"Alice","email":"alice@example.com"}

# Create user
curl -X POST http://localhost:4545/users -d '{"name":"Charlie"}'
# {"id":3,"name":"New User"}
```

---

## Pattern Matching with Regex

Match dynamic paths using regex:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4546,
    "protocol": "http",
    "stubs": [{
      "predicates": [{
        "matches": {
          "path": "/users/\\d+"
        }
      }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "body": { "message": "User found" }
        }
      }]
    }]
  }'
```

```bash
curl http://localhost:4546/users/123    # User found
curl http://localhost:4546/users/999    # User found
curl http://localhost:4546/users/abc    # No match: 200 with an empty body (Mountebank behaviour)
```

---

## JSON Body Matching

Match requests based on JSON body content:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4547,
    "protocol": "http",
    "stubs": [
      {
        "predicates": [{
          "equals": {
            "method": "POST",
            "body": { "action": "login", "username": "admin" }
          }
        }],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": { "token": "abc123", "role": "admin" }
          }
        }]
      },
      {
        "predicates": [{
          "contains": {
            "body": { "action": "login" }
          }
        }],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": { "token": "xyz789", "role": "user" }
          }
        }]
      }
    ]
  }'
```

```bash
# Admin login (exact match)
curl -X POST http://localhost:4547/login \
  -H "Content-Type: application/json" \
  -d '{"action":"login","username":"admin"}'
# {"token":"abc123","role":"admin"}

# Regular user login (contains match)
curl -X POST http://localhost:4547/login \
  -H "Content-Type: application/json" \
  -d '{"action":"login","username":"bob"}'
# {"token":"xyz789","role":"user"}
```

---

## JSONPath Predicates

Match specific values in JSON using JSONPath:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4548,
    "protocol": "http",
    "stubs": [{
      "predicates": [{
        "jsonpath": { "selector": "$.order.total" },
        "equals": { "body": 100 }
      }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "body": { "status": "approved", "discount": "10%" }
        }
      }]
    }]
  }'
```

```bash
curl -X POST http://localhost:4548/checkout \
  -H "Content-Type: application/json" \
  -d '{"order":{"items":["a","b"],"total":100}}'
# {"status":"approved","discount":"10%"}
```

---

## Simulating Delays

Add latency to responses for testing timeouts:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4549,
    "protocol": "http",
    "stubs": [{
      "predicates": [{ "equals": { "path": "/slow" } }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "body": "Response after delay"
        },
        "_behaviors": {
          "wait": 2000
        }
      }]
    }]
  }'
```

```bash
time curl http://localhost:4549/slow
# Response after delay
# real 0m2.015s
```

---

## Error Simulation

Test error handling in your application:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4550,
    "protocol": "http",
    "stubs": [
      {
        "predicates": [{ "equals": { "path": "/error/400" } }],
        "responses": [{
          "is": {
            "statusCode": 400,
            "body": { "error": "Bad Request", "message": "Invalid input" }
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
            "headers": { "Retry-After": "60" },
            "body": { "error": "Service Unavailable" }
          }
        }]
      }
    ]
  }'
```

---

## Managing Imposters

### List All Imposters

```bash
curl http://localhost:2525/imposters
```

### Get Imposter Details

```bash
curl http://localhost:2525/imposters/4545
```

### Delete an Imposter

```bash
curl -X DELETE http://localhost:2525/imposters/4545
```

### Delete All Imposters

```bash
curl -X DELETE http://localhost:2525/imposters
```

---

## Query Parameter Filtering

Match requests based on query parameters:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4551,
    "protocol": "http",
    "stubs": [
      {
        "predicates": [
          { "endsWith": { "path": "/tasks" } },
          { "equals": { "query": { "status": "OPEN" } } },
          { "deepEquals": { "method": "GET" } }
        ],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": { "count": 3, "tasks": [{"id": 1, "status": "OPEN"}] }
          }
        }]
      },
      {
        "predicates": [
          { "endsWith": { "path": "/tasks" } },
          { "equals": { "query": { "status": "CLOSED" } } },
          { "deepEquals": { "method": "GET" } }
        ],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": { "count": 0, "tasks": [] }
          }
        }]
      }
    ]
  }'
```

```bash
curl "http://localhost:4551/tasks?status=OPEN"
# {"count":3,"tasks":[{"id":1,"status":"OPEN"}]}

curl "http://localhost:4551/tasks?status=CLOSED"
# {"count":0,"tasks":[]}
```

---

## Organizing Stubs with Scenarios

Use `scenarioName` to organize and document your stubs:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4552,
    "protocol": "http",
    "name": "User Service",
    "allowCORS": true,
    "stubs": [
      {
        "scenarioName": "UserService-GetUser-Success",
        "predicates": [
          { "matches": { "path": "/users/\\d+" } },
          { "deepEquals": { "method": "GET" } }
        ],
        "responses": [{
          "is": {
            "statusCode": 200,
            "headers": { "Content-Type": "application/json" },
            "body": { "id": 1, "name": "Alice", "email": "alice@example.com" }
          }
        }]
      },
      {
        "scenarioName": "UserService-GetUser-NotFound",
        "predicates": [
          { "equals": { "path": "/users/999" } },
          { "deepEquals": { "method": "GET" } }
        ],
        "responses": [{
          "is": {
            "statusCode": 404,
            "headers": { "Content-Type": "application/json" },
            "body": { "error": "User not found", "code": "USER_NOT_FOUND" }
          }
        }]
      }
    ]
  }'
```

The `scenarioName` field helps identify which test scenario each stub supports.

---

## CORS Support

Enable CORS for browser-based testing with `allowCORS`:

```json
{
  "port": 4545,
  "protocol": "http",
  "allowCORS": true,
  "stubs": [...]
}
```

This automatically adds CORS headers to responses and handles preflight OPTIONS requests.

---

## Example Files

Complete working examples are available in the [`examples/`](https://github.com/achird-labs/rift/tree/master/examples) directory:

| File | Description |
|:-----|:------------|
| `basic-api.json` | Simple REST API with CRUD operations |
| `error-testing.json` | Various HTTP error responses |
| `latency-testing.json` | Latency simulation for timeout testing |
| `task-management-api.json` | Complete task API with scenarios |
| `feature-flags-api.json` | Feature toggle service mock |
| `authentication-api.json` | Login/logout with token validation |

Load an example (from a checkout of the repository):

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @examples/task-management-api.json
```

---

## Next Steps

- [Predicates Reference]({{ site.baseurl }}/mountebank/predicates/) - All predicate types
- [Behaviors Guide]({{ site.baseurl }}/mountebank/behaviors/) - wait, decorate, copy
- [Proxy Mode]({{ site.baseurl }}/mountebank/proxy/) - Record and replay
