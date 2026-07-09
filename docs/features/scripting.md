---
layout: default
title: Scripting
parent: Features
nav_order: 2
---

# Scripting

Rift supports multiple scripting engines for dynamic behavior.

---

## Script API: unified `ctx`, `respond(ctx)`, result constructors

`_rift.script` has a single contract that is **identical across Rhai and JavaScript**: a `ctx`
object passed into the script, and result constructors instead of a hand-built
`#{ inject:, fault: }` map — see [`ctx` API](#ctx-api) below for the full reference.

```rhai
// named entrypoint
fn respond(ctx) {
  let n = ctx.state.incr("attempts");
  if n <= 2 {
    http(503, #{ error: "unavailable", attempt: n }).header("Retry-After", "1")
  } else {
    http(200, #{ ok: true, succeededOnAttempt: n })
  }
}
```

```rhai
// bare-expression form — no `fn respond(ctx) { ... }` wrapper at all. The whole script body
// IS the function, with `ctx` already in scope. Equivalent to the named form above.
let n = ctx.state.incr("attempts");
if n <= 2 {
  http(503, #{ error: "unavailable", attempt: n }).header("Retry-After", "1")
} else {
  http(200, #{ ok: true, succeededOnAttempt: n })
}
```

Both forms are legal for every hook placement; which named entrypoint applies depends on where the
script is attached — see the table below.

---

## Available Engines

| Engine | Format | Use Case |
|:-------|:-------|:---------|
| **JavaScript** | `inject` response | Mountebank-compatible injection responses |
| **Rhai** | `_rift.script` | Lightweight fault logic with flow state |

---

## JavaScript (Mountebank Inject)

JavaScript uses the standard Mountebank `inject` response format for compatibility.

### Injection Responses

```json
{
  "responses": [{
    "inject": "function(config) { return { statusCode: 200, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ path: config.request.path, timestamp: Date.now() }) }; }"
  }]
}
```

### Request Object

```javascript
config.request.method      // "GET", "POST", etc.
config.request.path        // "/api/users/123"
config.request.query       // { page: "1", limit: "10" }
config.request.headers     // { "content-type": "application/json" }
config.request.body        // Request body (string or parsed object)
```

> Path parameters (`request.pathParams`, from a stub's [`routePattern`](../configuration/native/#route-patterns-routepattern)) are exposed to the `_rift.script` engines — Rhai and JavaScript — not to this Mountebank `inject` `config.request` object.

### State Object

Persist data across requests within the same imposter:

```javascript
function(config, state) {
  // Initialize or increment counter
  state.counter = (state.counter || 0) + 1;

  // Store user-specific data
  var userId = config.request.headers['X-User-Id'];
  state.users = state.users || {};
  state.users[userId] = { lastSeen: Date.now() };

  return {
    statusCode: 200,
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ requestNumber: state.counter })
  };
}
```

---

## Rhai (`_rift.script`)

Rhai is a lightweight embedded scripting language optimized for Rust. Scripts define a
`respond(ctx)` function, or the bare-expression form — see [`ctx` API](#ctx-api) below.

### Basic Script

```json
{
  "port": 4545,
  "protocol": "http",
  "_rift": {
    "flowState": {"backend": "inmemory", "ttlSeconds": 600}
  },
  "stubs": [{
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "fn respond(ctx) { let count = ctx.state.incr(\"counter\"); http(200, #{ count: count }) }"
        }
      }
    }]
  }]
}
```

### Available Variables

See [`ctx.request`](#ctxrequest) for the full reference:

```rhai
// Request information
ctx.request.method        // String: "GET", "POST", etc.
ctx.request.path          // String: "/api/users"
ctx.request.headers       // Map with lowercased keys: access via ctx.request.headers["header-name"]
ctx.request.header(name)  // case-insensitive getter, e.g. ctx.request.header("X-Name")
ctx.request.query         // Map: access via ctx.request.query["param"]
ctx.request.pathParams    // Map: access via ctx.request.pathParams["name"] (populated from the stub's routePattern)
ctx.request.body          // Raw request body (string)
ctx.request.json          // Body lazily parsed as JSON (unit if not JSON)

// Helper functions
timestamp_header()      // RFC 1123 formatted timestamp for HTTP Date header
```

### State

State persists across requests, automatically scoped to the request's resolved flow id — no
explicit id argument needed. See [`ctx.state` and `ctx.store`](#ctxstate-and-ctxstore) below for
the full reference.

```rhai
// Get value (get_or supplies a default instead of the () / nil dance)
let count = ctx.state.get_or("counter", 0);

// Set value
ctx.state.set("counter", count + 1);

// Increment counter (returns new value)
let attempts = ctx.state.incr("attempts");

// Check existence
if ctx.state.exists("key") {
  // key exists
}

// Delete value
ctx.state.delete("key");

// Set TTL for the flow (seconds)
ctx.state.ttl(300);
```

### Return Values

`respond(ctx)` returns a result constructor — see [Result constructors](#result-constructors)
below for the full reference:

```rhai
// No injection (pass through to next response or upstream)
pass()

// Inject error response
http(503, #{ error: "Service unavailable" }).header("Retry-After", "30")

// Inject latency
delay(500)
```

---

## Script Examples

### Rate Limiting

```json
{
  "_rift": {
    "flowState": {"backend": "inmemory", "ttlSeconds": 60}
  },
  "stubs": [{
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "fn respond(ctx) { let count = ctx.state.incr(\"requests\"); if count > 100 { http(429, #{ error: \"Rate limit exceeded\", count: count }).header(\"Retry-After\", \"60\") } else { pass() } }"
        }
      }
    }]
  }]
}
```

### Retry Simulation (Fail First N Requests)

```json
{
  "_rift": {
    "flowState": {"backend": "inmemory", "ttlSeconds": 300, "flowIdSource": "header:X-Flow-Id"}
  },
  "stubs": [{
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "fn respond(ctx) { let attempts = ctx.state.incr(\"attempts\"); if attempts <= 2 { http(503, #{ error: \"Temporary failure\", attempt: attempts }) } else { pass() } }"
        }
      }
    }]
  }]
}
```

### Authoring Scripts: `file:` and `ref:` (YAML)

The retry script above works fine as a single JSON-escaped line, but it stops being readable once
a script grows past a few statements. `_rift.script` also accepts `file:` (load the script from a
separate file) and `ref:` (resolve from a named entry under `_rift.scripts`) instead of inline
`code:` — exactly one of `code`, `file`, or `ref` must be set. This is most useful in a YAML
configfile, where a block scalar (`|`) lets you write the same script as normal multi-line Rhai.
`rift --configfile config.yaml` expects a YAML *sequence* of imposters at the document root (a
single imposter is still a one-element sequence):

```yaml
- port: 4545
  protocol: http
  _rift:
    flowState: { backend: inmemory, ttlSeconds: 300, flowIdSource: "header:X-Flow-Id" }
  stubs:
    - responses:
        - _rift:
            script:
              engine: rhai
              # Block scalar: the exact retry logic from the JSON example above, just readable.
              code: |
                fn respond(ctx) {
                  let attempts = ctx.state.incr("attempts");
                  if attempts <= 2 {
                    http(503, #{ error: "Temporary failure", attempt: attempts })
                  } else {
                    pass()
                  }
                }
```

For a script reused across stubs — or one you'd rather keep in its own file for editor
syntax-highlighting and diffs — use `file:` instead, resolved relative to the configfile's own
directory (`--datadir` files resolve the same way; admin-API-created imposters resolve under
`--scripts-dir` instead, and reject any path that escapes it):

```yaml
- port: 4545
  protocol: http
  _rift:
    flowState: { backend: inmemory, ttlSeconds: 300 }
    # Named registry: give a script a name once, `ref:` it from any response.
    scripts:
      failTwice:
        file: scripts/fail-twice.rhai   # engine inferred from the extension: .rhai -> rhai
  stubs:
    - responses:
        - _rift:
            script:
              ref: failTwice
```

`engine` is inferred from `file`'s extension (`.rhai` -> `rhai`, `.js` ->
`javascript`) when omitted; a `ref:` may not itself point at another `ref:` (no chains), and an
unknown `ref:` or a `file:` that can't be read is a config-time validation error — surfaced at
`rift --configfile` load, at `POST /imposters` as a `400`, and by `rift-lint`.

### Counter with Multiple Endpoints

```json
{
  "_rift": {
    "flowState": {"backend": "inmemory", "ttlSeconds": 600}
  },
  "stubs": [
    {
      "predicates": [{"equals": {"method": "POST", "path": "/api/counter/increment"}}],
      "responses": [{
        "_rift": {
          "script": {
            "engine": "rhai",
            "code": "fn respond(ctx) { let counter = ctx.state.incr(\"counter\"); http(200, #{ counter: counter }) }"
          }
        }
      }]
    },
    {
      "predicates": [{"equals": {"method": "GET", "path": "/api/counter"}}],
      "responses": [{
        "_rift": {
          "script": {
            "engine": "rhai",
            "code": "fn respond(ctx) { let counter = ctx.state.get_or(\"counter\", 0); http(200, #{ counter: counter }) }"
          }
        }
      }]
    },
    {
      "predicates": [{"equals": {"method": "DELETE", "path": "/api/counter"}}],
      "responses": [{
        "_rift": {
          "script": {
            "engine": "rhai",
            "code": "fn respond(ctx) { ctx.state.delete(\"counter\"); http(200, #{ message: \"Counter reset\" }) }"
          }
        }
      }]
    }
  ]
}
```

---

## `ctx` API

`ctx` is built the same way, with the same field names and semantics, in every engine and at every
hook placement. Placement determines which entrypoint the engine looks for:

| Placement | Entrypoint | Returns |
|:----------|:-----------|:--------|
| Response script (`_rift.script`) | `respond(ctx)` | a result constructor, or nothing (pass through) |
| Predicate script | `matches(ctx)` | `true`/`false` |
| Decorate behavior | `transform(ctx)` | a result constructor describing the new response, or nothing (no change) |
| Wait behavior | `delay(ctx)` | a number of milliseconds |

For each placement, the script may either define the named function explicitly, or omit the
wrapper entirely and write the function body directly at the top level (bare-expression form) —
both are shown above for `respond`.

### `ctx.request`

```
ctx.request.method        // "GET", "POST", etc.
ctx.request.path          // "/api/users/123"
ctx.request.pathParams    // map: populated from the stub's routePattern
ctx.request.query         // map of query-string parameters
ctx.request.headers       // map with LOWERCASED keys — always, regardless of wire casing
ctx.request.header(name)  // case-insensitive getter, e.g. ctx.request.header("X-Flow-Id")
ctx.request.body          // the raw request body, as a string
ctx.request.json          // the body lazily parsed as JSON; unit/nil/null if it isn't JSON
```

`ctx.request.header(name)` exists so `X-Flow-Id`, `x-flow-id`, and `X-FLOW-ID` all resolve the same
value — a common source of bugs when reading `request.headers[...]` directly against on-the-wire
casing.

### `ctx.response`

Available **only** on the decorate/`transform(ctx)` hook — `undefined`/`nil`/absent everywhere
else. Same shape as `ctx.request`, describing the in-flight response instead:

```
ctx.response.status        // number
ctx.response.headers       // map, lowercased keys
ctx.response.header(name)  // case-insensitive getter
ctx.response.body          // raw string
ctx.response.json          // lazily parsed JSON, or unit/nil/null
```

### `ctx.state` and `ctx.store`

`ctx.state` is a flow-scoped state handle, already bound to the request's resolved flow id (per
`flowIdSource` — the same resolution the scenario-state gate uses):

```rhai
let n = ctx.state.get("key");     // unit if not set
ctx.state.set("key", n + 1);
let attempts = ctx.state.incr("attempts");
ctx.state.exists("key");          // bool
ctx.state.delete("key");
```

Atomic ops and ergonomic getters:

```rhai
let n = ctx.state.get_or("attempts", 0);   // value, or the default if absent — kills
                                            // the `if v == () { v = 0; }` idiom
let n = ctx.state.incr_by("attempts", 5);  // atomic +5, starts at 0 when absent
ctx.state.ttl(60);                         // per-flow TTL override, in seconds

let outcome = ctx.state.cas("status", "pending", "paid");
if outcome.applied {
  // this call won the race
} else {
  // outcome.current is who won instead (unit/nil/null if the key was absent)
}
```

`cas(key, expected, new)` is Rift's atomic compare-and-set. It always returns an
**object** — `{ applied: true }` on success, or `{ applied: false, current: <value> }` on conflict —
rather than a bare value, so a conflicting stored value that happens to equal `true` can never be
mistaken for "applied". This shape is identical in both engines; only the method spelling
differs to match each engine's naming convention — Rhai uses `get_or`/`incr_by` (snake_case), JS
uses `getOr`/`incrBy` (camelCase); `cas` and `ttl` are spelled the same everywhere.

Every `ctx.state` call is fail-loud: a store failure (a Redis connection dropping mid-request, for
example) raises a script error and is logged — it is never silently swallowed into a default value.
See [Flow State]({{ site.baseurl }}/features/flow-state/) for how the underlying store is selected,
including the in-memory auto-provisioning that lets `ctx.state` work with zero configuration.

`ctx.store` is the escape hatch for touching a *different* flow's state — `ctx.store.flow(id)`
returns a handle just like `ctx.state`, but scoped to `id` instead of the request's own flow:

```rhai
ctx.store.flow("other-flow-id").set("shared", 99);
```

### `ctx.flowId` and `ctx.stub`

```
ctx.flowId              // the resolved flow id (string) — same value ctx.state is bound to
ctx.stub.scenarioName    // string, or unit/nil/null if the stub isn't part of a scenario
ctx.stub.scenarioState   // string, or unit/nil/null
ctx.stub.id              // the stub's own id, or unit/nil/null if it has none
```

### `ctx.logger`

Real logging (not a no-op): `debug`/`info`/`warn`/`error`, routed to the process's own tracing
output at target `rift::script`, tagged with the imposter port and stub id where available.

```rhai
ctx.logger.info("handling request " + ctx.request.path);
```

### Result constructors

How `respond(ctx)`/`transform(ctx)` describe what should happen — no hand-built
`#{ inject:, fault: }` map. Available in `respond(ctx)`/`transform(ctx)` (and as the return value
of a bare-expression script for those placements):

| Constructor | Meaning |
|:------------|:--------|
| `http(status)` / `http(status, body)` | respond with this status/body; chain `.header(k, v)` for extra headers |
| `delay(ms)` | inject latency, then respond normally |
| `reset()` | reset the connection (transport-level) |
| `pass()` | respond normally, no injection |
| *(nothing)* | same as `pass()` |

`http`'s body is a **value**, not a hand-assembled JSON string: pass a map/array and it is
JSON-serialized with `Content-Type: application/json` set automatically (unless you set your own
`Content-Type` via `.header(...)`, which always wins); pass a string and it's used verbatim.

```rhai
// Object body -> JSON + Content-Type: application/json
http(503, #{ error: "unavailable", attempt: 2 })

// String body -> passed through as-is, no Content-Type added
http(200, "OK")

// Chained headers
http(429, #{ error: "rate limited" }).header("Retry-After", "60")
```

---

## Execution Limits

`_rift.script` execution is bounded so a runaway script cannot wedge the engine.

- **Wall-clock timeout.** Each script runs under a deadline — `_rift.scriptEngine.timeoutMs` if
  configured, otherwise **5000 ms**. Rhai is interrupted mid-run when the deadline passes.
- **JavaScript (Boa) bounds.** The Boa interpreter cannot be interrupted per-instruction, so it is
  bounded structurally instead: a loop-iteration limit of **10,000,000 iterations per call frame**
  and a recursion limit of **512**. The client is still released at the wall-clock timeout with an
  error; a pathological nested loop may keep a background worker busy a little longer, but it cannot
  run unbounded.
- **On timeout or error** — a compile error, a runtime error, or exceeding a bound — the response is
  `500 Internal Server Error` carrying an `x-rift-script-error: true` header.

```json
{
  "_rift": {
    "scriptEngine": { "timeoutMs": 2000 }
  }
}
```

`_rift.scriptEngine.defaultEngine` sets which engine runs a `_rift.script` block when the block
itself omits `engine`: `"rhai"` (default) or `"javascript"`. A per-script `engine` field
always takes precedence over `defaultEngine`.

```json
{
  "_rift": {
    "scriptEngine": { "defaultEngine": "javascript" }
  }
}
```

## Flow-Store Error Semantics

`ctx.state` is always fail-loud: every op — `get`/`set`/`incr`/`exists`/`delete` and the atomic
`get_or`/`incr_by`/`cas`/`ttl` — **raises** a script error on a backend failure (e.g. a Redis
outage mid-request) and logs it, so a store outage is never silently returned as an empty/absent
value. The raised error propagates to the standard script-error path (`500` with
`x-rift-script-error`).

---

## Engine Comparison

| Feature | JavaScript | Rhai |
|:--------|:-----------|:-----|
| Format | `inject` response | `_rift.script` |
| State access | `state.key` | `ctx.state.get(key)` |
| Flow isolation | Per imposter | Per flow_id |
| Function wrapper | None needed | `respond(ctx)`/bare |
| Performance | Good | Excellent |
| Mountebank compatible | Yes | No |

---

## Performance Tips

1. **Use Rhai for high-throughput** - it is compiled and cached for efficient reuse
2. **Minimize `ctx.state` access** - Each get/set has overhead; batch operations when possible
3. **Keep scripts simple** - Complex logic is harder to debug and maintain
4. **Choose `flowIdSource` wisely** - it determines what `ctx.state` isolates by (request header,
   imposter port); pick a source that keys state per request/user/session to avoid collisions
5. **Set appropriate TTLs** - Prevent unbounded state growth with `ttlSeconds` config
