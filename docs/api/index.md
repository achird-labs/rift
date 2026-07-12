---
layout: default
title: API Reference
nav_order: 7
permalink: /api/
---

# REST API Reference

Rift provides a Mountebank-compatible REST API for managing imposters.

---

## Base URL

```
http://localhost:2525
```

---

## Authentication

When Rift is started with `--api-key <TOKEN>` (or `MB_APIKEY`), every admin API request must send the
token in the `Authorization` header. Requests without a matching token receive `401 Unauthorized`.
Data-plane traffic — direct imposter ports and the `/__rift/:port/...` gateway — is not gated.

```bash
curl -H "Authorization: <TOKEN>" http://localhost:2525/imposters
```

---

## Root

### GET /

Get API information and links.

**Response:**
```json
{
  "_links": {
    "imposters": { "href": "/imposters" },
    "config": { "href": "/config" },
    "logs": { "href": "/logs" }
  }
}
```

---

## Imposters

### GET /imposters

List all imposters.

**Query Parameters:**
- `replayable` (boolean) - Include full stub details for export

**Response:**
```json
{
  "imposters": [
    {
      "port": 4545,
      "protocol": "http",
      "name": "User Service",
      "numberOfRequests": 42
    },
    {
      "port": 4546,
      "protocol": "https",
      "name": "Payment Service",
      "numberOfRequests": 15
    }
  ]
}
```

**Example:**
```bash
curl http://localhost:2525/imposters
curl "http://localhost:2525/imposters?replayable=true"
```

---

### POST /imposters

Create a new imposter.

**Request Body:**
```json
{
  "port": 4545,
  "protocol": "http",
  "name": "My Service",
  "stubs": [
    {
      "predicates": [{ "equals": { "path": "/test" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
    }
  ]
}
```

**Response:** `201 Created`
```json
{
  "port": 4545,
  "protocol": "http",
  "name": "My Service",
  "numberOfRequests": 0,
  "stubs": [...]
}
```

**Example:**
```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "stubs": [{
      "responses": [{ "is": { "statusCode": 200 } }]
    }]
  }'
```

---

### PUT /imposters

Replace all imposters (bulk create/update). The running set is *reconciled* toward the payload —
the same incremental engine as `POST /admin/reload` — rather than deleted wholesale and recreated:
imposters absent from the payload are deleted, changed ones are replaced (or stub-patched), and an
imposter whose config is unchanged keeps its runtime state (recorded requests, response cycling).
The whole set is validated before anything is touched, so an invalid payload never disturbs the
running imposters. Use `DELETE /imposters` first if you also want unchanged imposters reset.

**Request Body:**
```json
{
  "imposters": [
    { "port": 4545, "protocol": "http", "stubs": [...] },
    { "port": 4546, "protocol": "http", "stubs": [...] }
  ]
}
```

**Response:** `200 OK`
```json
{
  "imposters": [...]
}
```

**Errors:**
- `400 Bad Request` — the set failed validation (bad protocol, duplicate port, duplicate stub id);
  the running imposters are unchanged.
- `500 Internal Server Error` — one or more imposters failed to apply (e.g. a port bind failure);
  the body carries the per-port `failed` list plus the `created`/`replaced`/`stubPatched`/`deleted`
  report of what did apply, mirroring `POST /admin/reload`.

---

### GET /imposters/{port}

Get imposter details.

**Query Parameters:**
- `replayable` (boolean) - Include full configuration for export
- `removeProxies` (boolean) - Exclude proxy stubs

**Response:**
```json
{
  "port": 4545,
  "protocol": "http",
  "name": "My Service",
  "numberOfRequests": 42,
  "requests": [
    {
      "method": "GET",
      "path": "/test",
      "headers": {...},
      "timestamp": "2024-01-15T10:30:00.000Z"
    }
  ],
  "stubs": [...]
}
```

**Example:**
```bash
curl http://localhost:2525/imposters/4545
curl "http://localhost:2525/imposters/4545?replayable=true"
```

---

### DELETE /imposters/{port}

Delete an imposter.

**Query Parameters:**
- `replayable` (boolean) - Return imposter config before deletion

**Response:** `200 OK`
```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [...]
}
```

**Example:**
```bash
curl -X DELETE http://localhost:2525/imposters/4545
```

---

### DELETE /imposters

Delete all imposters.

**Response:** `200 OK`
```json
{
  "imposters": [...]
}
```

**Example:**
```bash
curl -X DELETE http://localhost:2525/imposters
```

---

## Stub Management

### GET /imposters/{port}/stubs

List all stubs for an imposter (with HATEOAS `_links`).

---

### POST /imposters/{port}/stubs

Add a stub to an existing imposter.

**Request Body:**
```json
{
  "stub": {
    "predicates": [{ "equals": { "path": "/new" } }],
    "responses": [{ "is": { "statusCode": 200 } }]
  },
  "index": 0
}
```

**Response:** `200 OK`

**Example:**
```bash
curl -X POST http://localhost:2525/imposters/4545/stubs \
  -H "Content-Type: application/json" \
  -d '{
    "stub": {
      "predicates": [{ "equals": { "path": "/new" } }],
      "responses": [{ "is": { "statusCode": 201 } }]
    }
  }'
```

---

### GET /imposters/{port}/stubs/{index}

Get a single stub by its array index.

---

### PUT /imposters/{port}/stubs/{index}

Replace a stub at a specific index.

**Request Body:**
```json
{
  "predicates": [{ "equals": { "path": "/updated" } }],
  "responses": [{ "is": { "statusCode": 200 } }]
}
```

---

### DELETE /imposters/{port}/stubs/{index}

Delete a stub at a specific index.

---

### Stub operations by stable id

Every stub has a stable `id` (auto-generated as a UUID when omitted). These endpoints address a stub
by that id instead of by positional index, so concurrent edits don't shift the target.

| Method | Path | Action |
|:-------|:-----|:-------|
| `GET` | `/imposters/{port}/stubs/by-id/{id}` | Get the stub with this id |
| `PUT` | `/imposters/{port}/stubs/by-id/{id}` | Replace the stub with this id (position preserved) |
| `DELETE` | `/imposters/{port}/stubs/by-id/{id}` | Delete the stub with this id |

```bash
curl http://localhost:2525/imposters/4545/stubs/by-id/6f1c...e2
```

---

## Imposter State

### POST /imposters/{port}/enable

Re-enable a disabled imposter.

### POST /imposters/{port}/disable

Disable an imposter — it stops matching stubs and returns a default response — without deleting it.

```bash
curl -X POST http://localhost:2525/imposters/4545/disable
curl -X POST http://localhost:2525/imposters/4545/enable
```

---

## Requests

### GET /imposters/{port}/savedRequests

Get recorded requests (if `recordRequests: true`). Also available under the alias
`GET /imposters/{port}/requests`.

**Query Parameters:**
- `match=header:<Name>=<Value>` — keep only requests carrying a matching header
- `match=flow_id=<Value>` — keep only requests whose resolved flow id matches

Multiple `match` clauses are AND-ed together.

**Response:** a JSON array of recorded requests. Each element carries `request_from` (the client
`ip:port`); `body` is present only when the request had one.
```json
[
  {
    "request_from": "127.0.0.1:52344",
    "method": "GET",
    "path": "/api/users",
    "query": {},
    "headers": {
      "host": "localhost:4545",
      "user-agent": "curl/7.88.0"
    },
    "timestamp": "2024-01-15T10:30:00.000Z"
  }
]
```

---

### POST /imposters/{port}/verify

Count — and optionally return — recorded requests matching a predicate set, evaluated by the
engine's own predicate engine rather than re-implemented per client. This is what an SDK's
`verify(match, times(n))` calls instead of fetching `savedRequests` and re-evaluating predicates
locally (where operators like `xpath`/`inject` are impractical) or shipping the whole journal over
the wire just to count it.

**Request body:**
```json
{
  "predicates": [ { "equals": { "path": "/api/users" } } ],
  "flowId": "tenant-a",
  "includeRequests": false,
  "includeClosest": false
}
```
- `predicates` — standard Mountebank/Rift predicate objects, AND-ed together (same semantics as a
  stub's `predicates`).
- `flowId` *(optional)* — scope the count to one space, resolved via the imposter's
  `flow_id_source` (the same scoping as `match=flow_id=<Value>` on `savedRequests`).
- `includeRequests` *(optional, default `false`)* — return the matching requests, not just the count.
- `includeClosest` *(optional, default `false`)* — return the best-scoring non-match — the request
  satisfying the most predicate clauses (ties resolve to the most recent) — with per-clause failure
  details, for rendering a readable diff on a failed verification.

An `inject` predicate requires the server to be started with `--allowInjection`; otherwise the
request is rejected with `400 invalid injection` (the same gate the stub endpoints apply).

**Response:**
```json
{
  "matched": 2,
  "total": 17,
  "requests": [ /* present only with includeRequests */ ],
  "closest": {
    "request": { /* the closest non-matching recorded request */ },
    "failedPredicates": [
      { "predicate": { "equals": { "path": "/api/users" } }, "actual": { "path": "/api/orders" } }
    ]
  }
}
```
`matched` counts requests matching every predicate; `total` is the number of recorded requests in
scope (after any `flowId` filter). `requests`/`closest` are present only when the corresponding
option is set.

---

### DELETE /imposters/{port}/savedRequests

Clear recorded requests. Also available under the alias `DELETE /imposters/{port}/requests`.
Accepts the same `match=` query parameters as the `GET`, in which case only matching requests are
removed.

---

### DELETE /imposters/{port}/savedProxyResponses

Clear responses recorded by proxy stubs (`proxyOnce` / `proxyAlways`), leaving the imposter's other
state intact.

---

## Scenarios

Declarative state machines (Mountebank/WireMock style) gate stubs by `requiredScenarioState` and
transition via `newScenarioState`. State is partitioned per flow id.

### GET /imposters/{port}/scenarios

List scenario states. Accepts an optional `?flowId=<id>` query parameter (defaults to the imposter port).

### PUT /imposters/{port}/scenarios/{name}/state

Arrange a scenario's state directly.

**Request Body:** `{ "state": "AWAITING_PAYMENT", "flowId": "order-42" }` (`flowId` optional)

### POST /imposters/{port}/scenarios/reset

Reset scenarios. **Request Body:** `{ "flowId": "order-42" }` (optional; omit to reset the default flow).

---

## Spaces (Correlated Isolation)

A "space" isolates stubs and state to a correlation id (`flowId`), so parallel test runs don't collide.

### POST /imposters/{port}/spaces/{flowId}/stubs

Add a stub scoped to this space.

### GET /imposters/{port}/spaces/{flowId}/stubs

List this space's stubs.

### GET /imposters/{port}/spaces/{flowId}

Inspect the space — its stubs, scenario state, and request count.

### DELETE /imposters/{port}/spaces/{flowId}

Tear down the space, removing its scoped stubs, recorded requests, and scenario state.

---

## Flow State

A per-flow key/value store backing stateful stubs (e.g. retry-then-succeed). These admin endpoints
inspect and arrange it directly.

| Method | Path | Action |
|:-------|:-----|:-------|
| `GET` | `/admin/imposters/{port}/flow-state/{flow_id}/{key}` | Read a value (404 if absent) |
| `PUT` | `/admin/imposters/{port}/flow-state/{flow_id}/{key}` | Set a value — body `{ "value": <any JSON> }` |
| `DELETE` | `/admin/imposters/{port}/flow-state/{flow_id}/{key}` | Delete a key |

---

## Gateway

### /__rift/{port}/&lt;path&gt;

Dispatch any request to the imposter on `{port}`, rewriting the URI to `/<path>`. Lets a
containerized Rift publish only the admin port while still reaching every imposter. Works with any
HTTP method and is not gated by `--api-key`.

```bash
# equivalent to hitting the imposter on port 4545 at /api/users
curl http://localhost:2525/__rift/4545/api/users
```

---

## System

### GET /health

Liveness check. Returns `{"status":"ok"}`.

### GET /metrics

Prometheus-format metrics (imposter count, per-imposter request counts). Also exposed on the
dedicated metrics port (`--metrics-port`, default 9090).

### POST /admin/reload

Hot-reload imposters from the startup config source (`--configfile` / `--datadir`), replacing all
running imposters atomically. A no-op (200) when no config source was provided. New config is
validated before running imposters are torn down.

---

## Configuration

### GET /config

Get current configuration.

**Response:**
```json
{
  "options": {
    "port": 2525,
    "allowInjection": true,
    "localOnly": false
  }
}
```

---

## Logs

### GET /logs

Get server logs (if logging enabled).

**Query Parameters:**
- `startIndex` (number) - Start from this log entry
- `endIndex` (number) - End at this log entry

---

## Error Responses

### 400 Bad Request

Invalid request body or parameters.

```json
{
  "errors": [
    {
      "code": "bad data",
      "message": "invalid JSON"
    }
  ]
}
```

### 404 Not Found

Imposter doesn't exist.

```json
{
  "errors": [
    {
      "code": "no such resource",
      "message": "Imposter not found on port 4545"
    }
  ]
}
```

### 409 Conflict

Port already in use.

```json
{
  "errors": [
    {
      "code": "port conflict",
      "message": "Port 4545 is already in use"
    }
  ]
}
```

### 413 Payload Too Large

The request body exceeds the admin API's size limit (64 MiB). The limit bounds
how much of a single request Rift buffers into memory, since the admin plane
binds `0.0.0.0` and `--apikey` is optional.

```json
{
  "errors": [
    {
      "code": "413",
      "message": "Request body exceeds the 67108864-byte admin API limit"
    }
  ]
}
```

---

## Common Patterns

### Export and Reimport

```bash
# Export
curl "http://localhost:2525/imposters?replayable=true" > imposters.json

# Clear
curl -X DELETE http://localhost:2525/imposters

# Reimport
curl -X PUT http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d @imposters.json
```

### Verify Requests

```bash
# Create imposter with recording
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "recordRequests": true,
    "stubs": [...]
  }'

# Run tests...

# Verify requests
curl http://localhost:2525/imposters/4545 | jq '.requests'
```
