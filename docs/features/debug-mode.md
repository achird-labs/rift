---
layout: default
title: Debug Mode
parent: Features
nav_order: 6
---

# Debug Mode

Rift provides a debug mode that returns stub matching information instead of executing the actual response. This is useful for diagnosing why requests match (or don't match) specific stubs.

**Note**: This is a **Rift extension**. Mountebank does not provide this feature.

---

## Overview

When you send a request with the `X-Rift-Debug: true` header, Rift will:

1. Find the matching stub (if any)
2. Return detailed match information as JSON
3. **Not** execute the actual response

This allows you to see exactly which stub would handle a request without side effects.

---

## Usage

Add the `X-Rift-Debug` header to any request:

```bash
curl -H "X-Rift-Debug: true" http://localhost:4545/api/users
```

### Header Values

Debug mode is on when the header's (first) value is `true`, compared case-insensitively, or `1`.
The header name itself is matched case-insensitively. Any other value (`yes`, `on`, `0`) sends the
request down the normal path.

---

## Response Format

### When a Stub Matches

```json
{
  "debug": true,
  "request": {
    "method": "GET",
    "path": "/api/users",
    "query": "page=1",
    "headers": {
      "Accept": "*/*",
      "Host": "localhost:4545"
    },
    "body": null
  },
  "imposter": {
    "port": 4545,
    "name": "User Service",
    "protocol": "http",
    "stubCount": 3
  },
  "matchResult": {
    "matched": true,
    "stubIndex": 0,
    "stubId": "get-users",
    "predicates": [
      {"equals": {"method": "GET", "path": "/api/users"}}
    ],
    "responsePreview": {
      "responseType": "is",
      "statusCode": 200,
      "headers": {"Content-Type": "application/json"},
      "bodyPreview": "[{\"id\": 1, \"name\": \"Alice\"}]"
    }
  }
}
```

### When No Stub Matches

When no stub predicates match the request, the response includes all configured stubs for inspection.
`reason` is `No stub predicates matched the request`, or `No stubs configured for this imposter`
when the imposter has none:

```json
{
  "debug": true,
  "request": {
    "method": "GET",
    "path": "/unknown/path",
    "headers": {...}
  },
  "imposter": {
    "port": 4545,
    "name": "User Service",
    "protocol": "http",
    "stubCount": 3
  },
  "matchResult": {
    "matched": false,
    "reason": "No stub predicates matched the request",
    "allStubs": [
      {
        "index": 0,
        "id": "get-users",
        "predicates": [{"equals": {"method": "GET", "path": "/api/users"}}],
        "responseCount": 1
      },
      {
        "index": 1,
        "id": "create-user",
        "predicates": [{"equals": {"method": "POST", "path": "/api/users"}}],
        "responseCount": 1
      },
      {
        "index": 2,
        "predicates": [{"startsWith": {"path": "/api"}}],
        "responseCount": 1
      }
    ]
  }
}
```

---

## Response Fields

### Top Level

| Field | Type | Description |
|:------|:-----|:------------|
| `debug` | boolean | Always `true` for debug responses |
| `request` | object | Information about the incoming request |
| `imposter` | object | Information about the imposter |
| `matchResult` | object | Stub matching result |

### Request Object

| Field | Type | Description |
|:------|:-----|:------------|
| `method` | string | HTTP method (GET, POST, etc.) |
| `path` | string | Request path |
| `query` | string | Query string (if present) |
| `headers` | object | Request headers (excluding X-Rift-Debug), one string per name — a repeated header shows its first value |
| `body` | string | Request body (if present) |

### Imposter Object

| Field | Type | Description |
|:------|:-----|:------------|
| `port` | number | Port the imposter listens on |
| `name` | string | Imposter name (if configured) |
| `protocol` | string | `http` or `https` |
| `stubCount` | number | Number of configured stubs |

### Match Result Object (When Matched)

| Field | Type | Description |
|:------|:-----|:------------|
| `matched` | boolean | `true` if a stub matched |
| `stubIndex` | number | Index of the matching stub (0-based) |
| `stubId` | string | Stub ID (if configured) |
| `predicates` | array | The matching stub's predicates |
| `responsePreview` | object | Preview of the response |

### Match Result Object (When Not Matched)

| Field | Type | Description |
|:------|:-----|:------------|
| `matched` | boolean | `false` when no stub matches |
| `reason` | string | Explanation of why no stub matched |
| `allStubs` | array | List of all configured stubs |

### Response Preview Object

| Field | Type | Description |
|:------|:-----|:------------|
| `responseType` | string | `is`, `proxy`, `inject`, `fault`, or `_rift` (a script-only `_rift` response) |
| `statusCode` | number | HTTP status code (for `is` responses) |
| `headers` | object | Response headers (for `is` responses); a multi-valued header is joined with `, ` |
| `bodyPreview` | string | For `is`, the first 500 characters of the body (a JSON body is serialized first). For the other types, a one-line summary: `Proxy to: <url>`, `JavaScript inject: <first 50 chars>`, `Fault: <name>`, or for `_rift` one of `Rift script response`, `Rift fault injection`, `Rift extension response` |

The preview is of the response the stub would serve **next** — it peeks at the response cycle
without advancing it.

---

## Examples

### Debugging a Matching Request

```bash
# Create an imposter with multiple stubs
curl -X POST http://localhost:2525/imposters -d '{
  "port": 4545,
  "protocol": "http",
  "stubs": [
    {
      "id": "get-user-123",
      "predicates": [{"equals": {"path": "/users/123"}}],
      "responses": [{"is": {"statusCode": 200, "body": "{\"id\": 123}"}}]
    },
    {
      "id": "list-users",
      "predicates": [{"startsWith": {"path": "/users"}}],
      "responses": [{"is": {"statusCode": 200, "body": "[]"}}]
    }
  ]
}'

# Debug which stub matches
curl -H "X-Rift-Debug: true" http://localhost:4545/users/123
```

Response shows stub index 0 (`get-user-123`) matches.

### Debugging a Non-Matching Request

```bash
curl -H "X-Rift-Debug: true" http://localhost:4545/orders/456
```

Response shows no match and lists all stubs so you can see why none matched.

### Debugging Why Wrong Stub Matches

```bash
# If /users/123 is matching the wrong stub, use debug mode to see:
curl -H "X-Rift-Debug: true" http://localhost:4545/users/123

# Response shows which stub actually matched and its predicates
# Common issues:
# - Catch-all stub before specific stub
# - startsWith matching before equals
# - Wrong method in predicate
```

---

## Use Cases

1. **Diagnosing Match Failures**
   - See why a request doesn't match any stub
   - View all stubs to identify missing or incorrect predicates

2. **Understanding First-Match-Wins**
   - See which stub wins when multiple could match
   - Identify stub ordering issues

3. **Verifying Stub Configuration**
   - Confirm the right response would be returned
   - Check response preview without executing

4. **CI/CD Integration**
   - Programmatically verify stub routing
   - Validate imposter configuration

---

## Response Headers

Debug responses include:
- `Content-Type: application/json`
- `X-Rift-Debug-Response: true` - Indicates this is a debug response

---

## Error Responses

Debug mode evaluates predicates the same way a normal request does, so a stub that runs a script can
fail or time out while being *inspected*. Both failure doors answer the standard JSON error envelope
with `Content-Type: application/json`:

| Situation | Status | Extra header | Body |
|:----------|:-------|:-------------|:-----|
| The matching run panicked | `500` | — | `{"errors":[{"code":"...","message":"Debug matching failed"}]}` |
| A predicate `inject` threw | `400` | — | `invalid predicate injection`, as on the normal path |
| A predicate `inject` missed its own deadline | `504` | `x-rift-script-timeout: true` | `predicate injection timeout`, as on the normal path |
| The matching run exceeded `_rift.scriptEngine.timeoutMs` | `504` | `x-rift-script-timeout: true` | `{"errors":[{"code":"...","message":"Debug matching timed out"}]}` |

The timeout answered `500` in earlier releases. It is now a `504`, consistent with every other script
deadline (see [Scripting](scripting.md)), so a transient deadline miss is distinguishable from a
permanent configuration error — a debug request that times out is worth retrying.

Note the distinction from a normal debug response: an error here means the *inspection itself*
failed, so there is no `X-Rift-Debug-Response: true` header and no match report.

---

## Mountebank Compatibility

| Feature | Mountebank | Rift |
|:--------|:-----------|:-----|
| Debug mode | No | Yes |
| X-Rift-Debug header | Ignored | Activates debug mode |
| Match information | N/A | Full details |
| All stubs listing | N/A | When no match |

The `X-Rift-Debug` header is a Rift extension. If sent to Mountebank, it will be ignored and the request will be processed normally.
