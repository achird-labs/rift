# Flow State

Flow state is a per-flow key/value store used to build stateful mocks — retry-then-succeed, call
counters, saga progress. It is keyed by `(flow_id, key)`, where the flow id is resolved exactly as
for [Spaces](spaces.md). Scripts read and write it through `ctx.state`;
`_rift.stateOps` (below) writes it declaratively, with no script at all; and a `{{ state.<key> }}`
token (under `_rift.templated`) reads it back into a response body or header.

---

## Configuration

```json
{
  "_rift": {
    "flowState": {
      "ttlSeconds": 300,
      "flowIdSource": "header:X-Flow-Id"
    }
  }
}
```

| Field | Default | Notes |
|:------|:--------|:------|
| `ttlSeconds` | `300` | Default entry TTL, in seconds. Must be `>= 1` (see [TTL semantics](#ttl-semantics)). |
| `flowIdSource` | `imposter_port` | How the flow id is derived (`imposter_port` or `header:<Name>`). |

An unrecognised key directly under `flowState` is collected as an option for an embedder-supplied
store and ignored by the built-in in-memory store. That includes a leftover `backend` key: the
`backend` option was removed, so it selects nothing.

### Scope

No external store sits on the request path. The engine ships the in-memory flow store only; a
deployment that needs scenario or flow state shared across processes is a
[rift-cluster](../deployment/kubernetes.md#shared-state-across-pods) deployment, whose
owner-replicated tier replaces this engine's former Redis backend (removed in 0.22.0). The
`FlowStore` / `FlowStoreProvider` seams stay, so an embedder can still
bring any store (see [SPI](../embedding/spi.md)).

### Invalid flowState is fail-loud

An explicit `flowState` block that can't be honored **fails imposter creation** with
`400 Bad Request` rather than silently misbehaving:

- A **non-positive `ttlSeconds`** (`< 1`) is rejected: a zero/negative default TTL would expire every
  write the instant it lands (or errors on the first write, on a networked store), so it's caught up front rather
  than misbehaving later.

Every imposter without a `flowState` block gets an in-memory store at the default TTL (300 s), so
the admin flow-state and scenario routes and the FFI always persist what they are given, even on an
imposter with no scenario, script or `stateOps` stub (and on stubs added later in place). Script and
`stateOps` stubs still log a warning that this state does not survive a restart, and `rift-lint`'s
`W014` still applies.

### No `flowState`, but a script, scenario, or `stateOps` stub — auto-provisioned in-memory

An imposter with a `_rift.script` stub (which might call `ctx.state` at runtime), a scenario stub,
or a `_rift.stateOps` block, but **no** `flowState` block, gets a real in-memory store
auto-provisioned at the default TTL (300s) — a `tracing::warn!` (target `rift::script` for a
script stub, `rift::state_ops` for `stateOps`) is logged so this doesn't go unnoticed, and
`rift-lint` flags the same condition statically as `W014`. State works out of the box; it just
doesn't persist across restarts or get shared across a cluster.

---

## Script API

Scripts read and write flow state through the `ctx.state` handle, which is **pre-scoped to the
request's resolved flow id** — no flow id is passed per call. See
[Scripting → `ctx.state` and `ctx.store`](scripting.md#ctxstate-and-ctxstore)
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

Every `ctx.state` call is **fail-loud**: a backend error (e.g. an embedder's networked store dropping mid-request) raises a
script error and is logged — it is never silently swallowed into a default. In JavaScript the
method names are camelCase (`getOr`/`incrBy`); `cas`/`ttl`/`clear` are spelled the same in both
engines.

### TTL semantics

TTL is a **per-key** attribute, and every backend applies the same rules:

- **Every write stamps the default TTL.** A `set`/`incr`/`incr_by`/applied-`cas` (re)stamps the key's
  expiry to `now + ttlSeconds`.
- **Writes reset the TTL.** `ttl(key, s)` overrides a key's expiry, but a *subsequent* write to that
  key re-stamps it back to the default `ttlSeconds` — TTLs are set by the last write (there is
  no sticky/`KEEPTTL` mode).
- **`seconds <= 0` expires immediately.** `ttl(key, 0)` (or negative) deletes the key now; flow-level
  `ttl(0)` expires every current key in the flow. This mirrors the Redis `EXPIRE` primitive's semantics, giving scripts both
  "shrink the lifetime" (`ttl(key, 5)`) and "kill now" (`ttl(key, 0)`) with one primitive.
- **Flow-level `ttl(seconds)` is a convenience** over the per-key primitive: it re-stamps every key
  *currently* in the flow.
- There is deliberately **no "no expiry" mode** — an unexpirable store is a memory leak by
  construction — so `ttlSeconds` is mandatory and must be `>= 1`.

---

## Example — fail twice, then succeed

Requires `--allow-injection` (a `_rift.script` is a scripting surface, gated like `inject`). The counter is keyed by the `X-Flow-Id` header so each caller retries
independently.

Using the `ctx.state` API, `ctx.state.incr("attempts")` is already scoped to the caller's flow id
(per `flowIdSource` below) — no flow-id prelude, no `if n == () { n = 0; }` default dance:

```json
{
  "port": 4506,
  "protocol": "http",
  "_rift": {
    "flowState": { "ttlSeconds": 300, "flowIdSource": "header:X-Flow-Id" }
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

## `_rift.stateOps` — declarative writes, no script

For the common case — bump a counter, remember a value — writing a script is more than the intent
needs. `_rift.stateOps` is a declarative alternative: an array of state mutations on an `is`
response's `_rift` block, run in order against the request's resolved flow id after the response is
rendered, just before it is written:

```json
{
  "_rift": {
    "stateOps": [
      { "op": "increment", "key": "hits" },
      { "op": "set", "key": "lastId", "value": "{{ request.query.id }}" },
      { "op": "delete", "key": "tmp" },
      { "op": "clearFlow" }
    ]
  }
}
```

| Op | Effect |
|:---|:-------|
| `{ "op": "increment", "key", "by"? }` | Add `by` (default `1`) to an integer key, creating it at `0`. Atomic on every backend that has one. |
| `{ "op": "set", "key", "value" }` | Set `key` to the rendered `value` template (`{{ }}` grammar, plus one extra head: `previousValue`). |
| `{ "op": "delete", "key" }` | Delete one key. |
| `{ "op": "clearFlow" }` | Delete every key in the flow. |

### Worked example — increment, then a `set` that reads it back

```json
{
  "port": 4507,
  "protocol": "http",
  "stubs": [{
    "predicates": [{ "equals": { "method": "GET", "path": "/api/resource" } }],
    "responses": [{
      "is": { "statusCode": 200, "body": "hits before this request: {{ state.hits }}" },
      "_rift": {
        "templated": true,
        "stateOps": [
          { "op": "increment", "key": "hits" },
          { "op": "set", "key": "trail", "value": "{{ previousValue }}|{{ state.hits }}" }
        ]
      }
    }]
  }]
}
```

The body's `{{ state.hits }}` reads the value from *before* this request's own `increment` — the
same "show the count, then bump it" order WireMock uses — because `stateOps` runs last, right
before the response is written. The `set` on `trail` mentions `previousValue`, so it is a bounded
compare-and-set loop rather than a plain write: concurrent requests never lose an update. A `set`
whose value reads its own key through `{{ state.<that key> }}` is a compare-and-set loop too; a
`set` that reads no prior value is a plain write. The loop gives up after 64 lost races and reports
the failure like any other op error (below).

`previousValue` is meaningful only inside a `stateOps` `set` value — the key's value before that
op, empty when it had none. Anywhere else (a response body or header) it renders empty.

### `is` responses only

`stateOps` belongs to an `is` response. A `proxy`, `inject`, or script-only (`_rift.script`)
response has its own means of touching state (a script reaches `ctx.state` directly); `stateOps` on
one of those never runs. On a `proxy`, `inject` or bare `fault` response the `_rift` block is kept and
read back, but it has no effect: `GET /imposters/:port` reports a
[`config_key_ignored`](stub-analysis.md#config_key_ignored) warning and
`rift-lint` reports `W017`. On a script-only `_rift` response the engine reports a
[`state_ops_never_runs`](stub-analysis.md#state_ops_never_runs)
stub-analysis warning. `rift-lint` does not flag that case.

### Not run when the response never serves

`stateOps` runs only for the response actually served. It does **not** run when:

- a `_rift.fault` (or bare `fault`) fires — a probabilistic fault therefore makes the ops
  probabilistic too, since they belong to the response that was pre-empted;
- a `strictBehaviors` failure serves the 500 instead;
- a matcher error prevents a response from being selected at all.

A `wait` behavior delays the ops along with the response it delays — it does not skip them.

### When an op fails

Ops are fail-loud but **not transactional**. Normally a failing op (a store error, a `set` value
that fails to render, a compare-and-set that lost 64 races) is logged at `warn` and the remaining
ops still run. With `RIFT_DEBUG` (or `--debug`) on, the first failing op
aborts the rest and the request is answered with a `500` naming it — the ops before it have
already been applied.

### What `set` stores

A rendered `value` that is exactly a canonical integer (`"42"`, `"-3"`, `"0"` — not `"007"`, not
`"4.5"`) is stored as a JSON number; anything else is stored as a string. This is what lets
`set hits "0"` seed a counter that a later `increment` continues from — an `increment` on a
*string* value would otherwise silently restart at `0` — and what keeps `{{ state.hits }}` reading
back as the number it looks like.

A key named after a scenario holds that scenario's state, so `set`, `delete` and `clearFlow` there
are scenario transitions, and move `Last-Modified` on a conditional stub the scenario gates.

### Auto-provisioning and `rift-lint`

Like a `_rift.script` stub, a `stateOps` block gets a real in-memory store auto-provisioned when no
`_rift.flowState` is configured (see "auto-provisioned in-memory" above) — it is never left on a store that
discards writes. `rift-lint`'s `W014` flags the same condition statically, so it's a
deliberate choice rather than a surprise at scale.

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

Note: every imposter has a real store. `_rift.flowState` configures it; without that block an
in-memory store at the default 300 s TTL is used, so values persist even on an imposter with no
scenario, script or `stateOps` stub.

> **Embedding over the C-ABI (non-Rust)**: a non-Rust host can read, write, delete, and clear flow-state
> keys with zero loopback HTTP via [FFI (C-ABI)](../embedding/ffi.md#admin-long-tail-over-ffi) —
> `rift_flow_state_get` / `rift_flow_state_put` / `rift_flow_state_delete` / `rift_flow_state_clear` mirror the admin-API
> calls above exactly (same `ImposterManager` calls, same JSON shapes).
