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
| `ttlSeconds` | `300` | Default entry TTL, in seconds. Must be `>= 1` (see [TTL semantics](#ttl-semantics)). |
| `flowIdSource` | `imposter_port` | How the flow id is derived (`imposter_port` or `header:<Name>`). |

With `backend: "redis"` you may also set `url`, `poolSize` (default 10), and `keyPrefix`
(default `"rift:"`).

### Backend configuration is fail-loud

An explicit `flowState` block that can't be honored now **fails imposter creation** with
`400 Bad Request` rather than silently downgrading to a no-op store:

- An **unknown backend** string (anything other than `inmemory` or `redis`) is rejected at
  construction.
- A **`redis` backend that can't be created** — no redis config block, a connection/pool failure, or
  a binary built without the `redis-backend` feature — fails creation too.
- A **non-positive `ttlSeconds`** (`< 1`) is rejected: a zero/negative default TTL would expire every
  write the instant it lands (and errors on the first Redis `SETEX`), so it's caught up front rather
  than misbehaving later.

Only imposters with genuinely **no state surface** stay on the silent no-op store: no `flowState`
block, no scenario stubs, and no `_rift.script` stub. Such an imposter never touches the store, so
there is nothing to auto-provision.

### No `flowState`, but a script or scenario stub — auto-provisioned in-memory

An imposter with a `_rift.script` stub (which might call `ctx.state` at runtime) or a
scenario stub, but **no** `flowState` block, gets a real in-memory store auto-provisioned at the
default TTL (300s) — a `tracing::warn!` (target `rift::script`) is logged so this doesn't go
unnoticed, and `rift-lint` flags the same condition statically as `E042`. State works out of the
box; it just doesn't persist across restarts or get shared across a cluster the way an explicit
`flowState` (especially `backend: "redis"`) would.

---

## Script API

Scripts read and write flow state through the `ctx.state` handle, which is **pre-scoped to the
request's resolved flow id** — no flow id is passed per call. See
[Scripting → `ctx.state` and `ctx.store`]({{ site.baseurl }}/features/scripting/#ctxstate-and-ctxstore)
for the full surface:

| Call | Result |
|:-----|:-------|
| `ctx.state.get(key)` | value, or `()` / `nil` / `null` if absent |
| `ctx.state.get_or(key, default)` | value, or `default` if absent |
| `ctx.state.set(key, value)` | store a value |
| `ctx.state.exists(key)` | bool |
| `ctx.state.delete(key)` | remove a key |
| `ctx.state.incr(key)` / `incr_by(key, n)` | atomic increment, returns the new number |
| `ctx.state.cas(key, expected, new)` | atomic compare-and-set — `{ applied: … }` |
| `ctx.state.ttl(seconds)` | re-stamp the TTL of **every** key currently in this flow |
| `ctx.state.ttl(key, seconds)` | set **one** key's TTL — `true` if it existed, `false` if absent; `seconds <= 0` deletes it |
| `ctx.state.clear()` | remove **every** key in this flow |
| `ctx.store.flow(id)` | a handle scoped to a **different** flow id (cross-flow access) |

Every `ctx.state` call is **fail-loud**: a backend error (e.g. Redis dropping mid-request) raises a
script error and is logged — it is never silently swallowed into a default. In JavaScript the
method names are camelCase (`getOr`/`incrBy`); `cas`/`ttl`/`clear` are spelled the same in both
engines.

### TTL semantics

TTL is a **per-key** attribute, and every backend applies the same rules:

- **Every write stamps the default TTL.** A `set`/`incr`/`incr_by`/applied-`cas` (re)stamps the key's
  expiry to `now + ttlSeconds`.
- **Writes reset the TTL.** `ttl(key, s)` overrides a key's expiry, but a *subsequent* write to that
  key re-stamps it back to the default `ttlSeconds` — TTLs are set by the last write (this is exactly
  what Redis `SETEX` does; there is no sticky/`KEEPTTL` mode).
- **`seconds <= 0` expires immediately.** `ttl(key, 0)` (or negative) deletes the key now; flow-level
  `ttl(0)` expires every current key in the flow. This mirrors Redis `EXPIRE`, giving scripts both
  "shrink the lifetime" (`ttl(key, 5)`) and "kill now" (`ttl(key, 0)`) with one primitive.
- **Flow-level `ttl(seconds)` is a convenience** over the per-key primitive: it re-stamps every key
  *currently* in the flow. On Redis it does an O(keys-in-flow) `SCAN` + `EXPIRE`; on both backends the
  observable result is identical.
- There is deliberately **no "no expiry" mode** — an unexpirable store is a memory leak by
  construction — so `ttlSeconds` is mandatory and must be `>= 1`.

---

## Example — fail twice, then succeed

Requires `--allow-injection`. The counter is keyed by the `X-Flow-Id` header so each caller retries
independently.

Using the `ctx.state` API, `ctx.state.incr("attempts")` is already scoped to the caller's flow id
(per `flowIdSource` below) — no flow-id prelude, no `if n == () { n = 0; }` default dance:

```json
{
  "port": 4506,
  "protocol": "http",
  "_rift": {
    "flowState": { "backend": "inmemory", "ttlSeconds": 300, "flowIdSource": "header:X-Flow-Id" }
  },
  "stubs": [{
    "predicates": [{ "equals": { "method": "GET", "path": "/api/resource" } }],
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "fn respond(ctx) { let n = ctx.state.incr(\"attempts\"); if n <= 2 { http(503, `try ${n}`) } else { http(200, `ok ${n}`) } }"
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

# Clear an entire flow (all keys) — the test-arrange/teardown tool for resetting a flow
# between scenario runs. Idempotent: clearing an absent/empty flow still returns 200.
curl -X DELETE http://localhost:2525/admin/imposters/4506/flow-state/t1
```

Note: an imposter gets a real store when `_rift.flowState` is configured, or it declares scenario
stubs, or it has a `_rift.script` stub (auto-provisioned in-memory); only an imposter
with none of those uses a no-op store where values never persist.

> **Embedding over the C-ABI (non-Rust)**: a non-Rust host can read, write, and delete flow-state
> keys with zero loopback HTTP via [FFI (C-ABI)]({{ site.baseurl }}/embedding/ffi/#admin-long-tail-over-ffi-scenario-state--correlated-spaces) —
> `rift_flow_state_get` / `rift_flow_state_put` / `rift_flow_state_delete` mirror the admin-API
> calls above exactly (same `ImposterManager` calls, same JSON shapes).
