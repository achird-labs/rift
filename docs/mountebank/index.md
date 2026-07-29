---
layout: default
title: Mountebank Compatibility
nav_order: 5.5
has_children: true
permalink: /mountebank/
---

# Mountebank Compatibility

Rift implements the [Mountebank](http://www.mbtest.org/) REST API and configuration format. This allows you to use Rift as a drop-in replacement for Mountebank with significantly better performance.

**Scope:** Rift imposters are **HTTP/HTTPS only**. Mountebank also supports `tcp` and `smtp`
imposters, which Rift rejects — see the [migration guide]({{ site.baseurl }}/getting-started/migration)
for the full compatibility table before you migrate. (This is separate from TCP *fault injection*,
which HTTP imposters do support.)

> New to the model? Start with [Concepts]({{ site.baseurl }}/concepts/) — it explains imposters,
> stubs, predicates, responses, and behaviors conceptually, then the Rift-specific stateful features.
> This section is the syntax-level reference for the Mountebank-compatible surface.

---

## Core Concepts

### Imposters

An **imposter** is a mock server listening on a specific port. Each imposter:
- Listens on a configurable port
- Handles HTTP or HTTPS protocol
- Contains one or more stubs for request matching

### Stubs

A **stub** defines how to respond to matching requests:
- **Predicates**: Rules to match incoming requests
- **Responses**: What to return when predicates match

### Predicates

**Predicates** define request matching criteria:
- `equals` - Exact match
- `contains` - Partial match
- `matches` - Regex match
- `exists` - Field existence check
- `jsonpath` - JSON path matching
- `xpath` - XML path matching
- `and`, `or`, `not` - Logical combinations

### Behaviors

**Behaviors** modify responses before sending:
- `wait` - Add latency
- `decorate` - Transform response with JavaScript
- `copy` - Copy request values to response
- `lookup` - Look up data from external sources

---

## Quick Example

Create an imposter with multiple stubs:

```json
{
  "port": 4545,
  "protocol": "http",
  "name": "User Service Mock",
  "stubs": [
    {
      "predicates": [{
        "equals": { "method": "GET", "path": "/health" }
      }],
      "responses": [{
        "is": { "statusCode": 200, "body": "OK" }
      }]
    },
    {
      "predicates": [{
        "and": [
          { "equals": { "method": "GET" } },
          { "matches": { "path": "/users/\\d+" } }
        ]
      }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "headers": { "Content-Type": "application/json" },
          "body": { "id": 1, "name": "User" }
        }
      }]
    },
    {
      "predicates": [{
        "equals": { "method": "POST", "path": "/users" }
      }],
      "responses": [{
        "is": {
          "statusCode": 201,
          "headers": { "Content-Type": "application/json" },
          "body": { "id": 999, "message": "Created" }
        },
        "_behaviors": {
          "wait": 100
        }
      }]
    }
  ]
}
```

---

## REST API

### Create Imposter

```bash
POST /imposters
Content-Type: application/json

{
  "port": 4545,
  "protocol": "http",
  "stubs": [...]
}
```

### List Imposters

```bash
GET /imposters
```

### Get Imposter

```bash
GET /imposters/{port}
```

### Delete Imposter

```bash
DELETE /imposters/{port}
```

### Delete All Imposters

```bash
DELETE /imposters
```

---

## Documentation Sections

- [Imposters]({{ site.baseurl }}/mountebank/imposters/) - Creating and configuring mock servers
- [Predicates]({{ site.baseurl }}/mountebank/predicates/) - Request matching rules
- [Responses]({{ site.baseurl }}/mountebank/responses/) - Response configuration
- [Behaviors]({{ site.baseurl }}/mountebank/behaviors/) - Response modification
- [Proxy Mode]({{ site.baseurl }}/mountebank/proxy/) - Recording and replaying
