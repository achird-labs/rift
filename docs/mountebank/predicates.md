---
layout: default
title: Predicates
parent: Mountebank Compatibility
nav_order: 2
---

# Predicates

Predicates define rules for matching incoming requests. When all predicates in a stub match, the stub's response is returned.

---

## Request Fields

Predicates can match on these request fields:

| Field | Description | Example |
|:------|:------------|:--------|
| `method` | HTTP method | `GET`, `POST`, `PUT`, `DELETE` |
| `path` | Request path | `/api/users` |
| `query` | Query parameters | `{ "page": "1" }` |
| `headers` | Request headers | `{ "Authorization": "Bearer..." }` |
| `body` | Request body | String or JSON object |

---

## Predicate Types

### equals

Exact match on request fields:

```json
{
  "equals": {
    "method": "GET",
    "path": "/users",
    "query": { "active": "true" }
  }
}
```

Match headers (case-insensitive by default):

```json
{
  "equals": {
    "headers": { "Content-Type": "application/json" }
  }
}
```

Match JSON body:

```json
{
  "equals": {
    "body": { "username": "admin", "password": "secret" }
  }
}
```

### deepEquals

Like `equals` but requires exact object structure (no extra fields):

```json
{
  "deepEquals": {
    "body": { "id": 1, "name": "Test" }
  }
}
```

Works on all request fields - useful for strict matching:

```json
{
  "deepEquals": {
    "method": "GET",
    "path": "/api/users",
    "body": ""
  }
}
```

### contains

Partial match - checks if value is contained:

```json
{
  "contains": {
    "path": "/api",
    "body": { "action": "create" }
  }
}
```

Match substring in query parameter values:

```json
{
  "contains": {
    "query": { "lenderIds": "Test" }
  }
}
```

This matches requests like `?lenderIds=TestUser` or `?lenderIds=MyTestValue`.

### startsWith

Match beginning of string:

```json
{
  "startsWith": {
    "path": "/api/v1"
  }
}
```

### endsWith

Match end of string:

```json
{
  "endsWith": {
    "path": ".json"
  }
}
```

### matches

Regular expression match:

```json
{
  "matches": {
    "path": "/users/\\d+",
    "headers": { "Authorization": "Bearer [A-Za-z0-9]+" }
  }
}
```

### exists

Check field existence:

```json
{
  "exists": {
    "headers": { "X-Api-Key": true },
    "query": { "debug": false }
  }
}
```

- `true` - Field must exist
- `false` - Field must not exist

---

## JSONPath Predicates

Match specific values in JSON bodies using JSONPath. The `jsonpath` selector is combined with a predicate operation:

```json
{
  "jsonpath": { "selector": "$.user.name" },
  "equals": { "body": "admin" }
}
```

A leading `$` is optional. A selector that does not start with `$` is treated as **root-relative**:
`user.name` is normalized to `$.user.name`, and `[0]` to `$[0]`. This applies wherever a `jsonpath`
selector is used — predicates and the `copy` behavior's `jsonpath` extraction alike.

### JSONPath Operators

```json
// Equals - match exact value
{ "jsonpath": { "selector": "$.count" }, "equals": { "body": 10 } }

// Contains - partial match
{ "jsonpath": { "selector": "$.tags" }, "contains": { "body": "important" } }

// Matches - regex pattern
{ "jsonpath": { "selector": "$.email" }, "matches": { "body": ".*@example\\.com" } }

// Exists - check field presence
{ "jsonpath": { "selector": "$.optional" }, "exists": { "body": true } }
```

### JSONPath Examples

```json
// Match array element
{ "jsonpath": { "selector": "$.items[0].name" }, "equals": { "body": "First Item" } }

// Match nested value
{ "jsonpath": { "selector": "$.user.address.city" }, "equals": { "body": "NYC" } }

// Match with filter
{ "jsonpath": { "selector": "$.items[?(@.price > 100)]" }, "exists": { "body": true } }
```

---

## XPath Predicates

Match values in XML bodies using XPath:

```json
{
  "xpath": {
    "selector": "//user/name",
    "equals": "admin"
  }
}
```

### XPath with Namespaces

```json
{
  "xpath": {
    "selector": "//ns:item/ns:price",
    "ns": { "ns": "http://example.com/schema" },
    "equals": "99.99"
  }
}
```

---

## Logical Operators

### and

All predicates must match:

```json
{
  "and": [
    { "equals": { "method": "POST" } },
    { "equals": { "path": "/api/users" } },
    { "contains": { "body": { "role": "admin" } } }
  ]
}
```

### or

At least one predicate must match:

```json
{
  "or": [
    { "equals": { "path": "/api/v1/users" } },
    { "equals": { "path": "/api/v2/users" } }
  ]
}
```

### not

Predicate must not match:

```json
{
  "not": {
    "equals": { "method": "DELETE" }
  }
}
```

### Complex Combinations

```json
{
  "and": [
    { "equals": { "method": "POST" } },
    {
      "or": [
        { "contains": { "body": { "type": "A" } } },
        { "contains": { "body": { "type": "B" } } }
      ]
    },
    {
      "not": {
        "exists": { "headers": { "X-Test-Skip": true } }
      }
    }
  ]
}
```

---

## Predicate Options

### caseSensitive

Enable case-sensitive matching (default: false):

```json
{
  "equals": { "path": "/API/Users" },
  "caseSensitive": true
}
```

### except

Exclude fields from matching:

```json
{
  "equals": { "body": { "id": 1, "name": "Test" } },
  "except": "body.timestamp"
}
```

---

## Common Patterns

### Match Any GET Request

```json
{ "equals": { "method": "GET" } }
```

### Match Path with ID

```json
{ "matches": { "path": "/users/[0-9a-f-]+" } }
```

### Match JSON Content-Type

```json
{
  "and": [
    { "equals": { "method": "POST" } },
    { "contains": { "headers": { "Content-Type": "application/json" } } }
  ]
}
```

### Match Authenticated Requests

```json
{ "exists": { "headers": { "Authorization": true } } }
```

### Match Query Parameters

```json
{
  "equals": {
    "query": { "page": "1", "limit": "10" }
  }
}
```

---

## Multiple Predicates (Implicit AND)

When you specify multiple predicates in a stub's `predicates` array, they are combined with implicit AND - all must match:

```json
{
  "predicates": [
    { "endsWith": { "path": "/lender-details" } },
    { "contains": { "query": { "lenderIds": "ALL" } } },
    { "deepEquals": { "method": "GET" } }
  ],
  "responses": [{ "is": { "statusCode": 200 } }]
}
```

This matches GET requests to paths ending in `/lender-details` with query parameter `lenderIds` containing "ALL".

---

## Stub Ordering

Stubs are evaluated in order. Place more specific predicates first:

```json
{
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/users/admin" } }],
      "responses": [{ "is": { "body": "Admin user" } }]
    },
    {
      "predicates": [{ "matches": { "path": "/users/.*" } }],
      "responses": [{ "is": { "body": "Regular user" } }]
    },
    {
      "predicates": [],
      "responses": [{ "is": { "statusCode": 404 } }]
    }
  ]
}
```
