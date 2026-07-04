---
layout: default
title: Flow State
parent: Features
nav_order: 22
---

# Flow State

Flow state is a per-flow key/value store that scripts read and write to build stateful mocks —
retry-then-succeed, call counters, saga progress. It is keyed by `(flow_id, key)`, where the flow id
is resolved exactly as for [Spaces]({{ site.baseurl }}/features/spaces/).

---

## Configuration

```json
{
  "_rift": {
    "flowState": {
      "backend": "inmemory",
      "ttlSeconds": 300,
      "flowIdSource": "header:X-Flow-Id"
    }
  }
}
```

| Field | Default | Notes |
|:------|:--------|:------|
| `backend` | `inmemory` | `inmemory` or `redis`. |
| `ttlSeconds` | `300` | Default entry TTL. |
| `flowIdSource` | `imposter_port` | How the flow id is derived (`imposter_port` or `header:<Name>`). |

With `backend: "redis"` you may also set `url`, `poolSize` (default 10), and `keyPrefix`
(default `"rift:"`).

### Backend configuration is fail-loud

An explicit `flowState` block that can't be honored now **fails imposter creation** with
`400 Bad Request` rather than silently downgrading to a no-op store:

- An **unknown backend** string (anything other than `inmemory` or `redis`) is rejected at
  construction (issue #381) — previously it logged a warning and became a no-op.
- A **`redis` backend that can't be created** — no redis config block, a connection/pool failure, or
  a binary built without the `redis-backend` feature — fails creation too (issue #369).

Only the *implicit* no-op case is silent: an imposter with **no** `flowState` block (and no scenario
stubs) uses a no-op store where values never persist, exactly as before.

---

## Script API

The `flow_store` handle is passed to scripts (Rhai shown; Lua uses `flow_store:get(...)` method
syntax). All operations take the flow id explicitly.

| Call | Result |
|:-----|:-------|
| `flow_store.get(flow_id, key)` | value, or `()` / `nil` if absent |
| `flow_store.set(flow_id, key, value)` | store a value |
| `flow_store.exists(flow_id, key)` | bool |
| `flow_store.delete(flow_id, key)` | remove a key |
| `flow_store.increment(flow_id, key)` | atomically increment, returns the new number |
| `flow_store.set_ttl(flow_id, ttl_seconds)` | override the TTL for a flow |
| `flow_store.last_error()` | last backend error (or `()` / `nil` / `null` if the last op succeeded) — see [Scripting → Flow-Store Error Semantics]({{ site.baseurl }}/features/scripting/#flow-store-error-semantics) |

---

## Example — fail twice, then succeed

Requires `--allow-injection`. The counter is keyed by the `X-Flow-Id` header so each caller retries
independently.

```json
{
  "port": 4506,
  "protocol": "http",
  "_rift": { "flowState": { "backend": "inmemory", "ttlSeconds": 300 } },
  "stubs": [{
    "predicates": [{ "equals": { "method": "GET", "path": "/api/resource" } }],
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "fn should_inject(request, flow_store) { let id = request.headers[\"x-flow-id\"]; if id == () { id = \"default\"; } let n = flow_store.get(id, \"attempts\"); if n == () { n = 0; } n += 1; flow_store.set(id, \"attempts\", n); if n <= 2 { #{ inject: true, fault: \"error\", status: 503, body: `try ${n}` } } else { #{ inject: true, fault: \"error\", status: 200, body: `ok ${n}` } } }"
        }
      }
    }]
  }]
}
```

```bash
curl -i -H 'X-Flow-Id: t1' http://localhost:4506/api/resource   # 503 try 1
curl -i -H 'X-Flow-Id: t1' http://localhost:4506/api/resource   # 503 try 2
curl -i -H 'X-Flow-Id: t1' http://localhost:4506/api/resource   # 200 ok 3
```

---

## Inspecting and arranging state (admin API)

```bash
# Read a value (404 if the key is absent)
curl http://localhost:2525/admin/imposters/4506/flow-state/t1/attempts

# Set a value directly
curl -X PUT http://localhost:2525/admin/imposters/4506/flow-state/t1/attempts \
  -d '{"value": 0}'

# Delete a key
curl -X DELETE http://localhost:2525/admin/imposters/4506/flow-state/t1/attempts
```

Note: an imposter gets a real store only when `_rift.flowState` is configured (or it declares
scenario stubs); otherwise a no-op store is used and values never persist.
