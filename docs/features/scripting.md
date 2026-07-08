---
layout: default
title: Scripting
parent: Features
nav_order: 2
---

# Scripting

Rift supports multiple scripting engines for dynamic behavior.

---

## Script API v2: unified `ctx`, `respond(ctx)`, result constructors

As of this release, `_rift.script` has a single contract that is **identical across Rhai, Lua, and
JavaScript**: a `ctx` object passed into the script, and result constructors instead of a hand-built
`#{ inject:, fault: }` map. This is the recommended way to write new scripts — see
[`ctx` API v2](#ctx-api-v2) below for the full reference.

The **v1 `should_inject(request, flow_store)` wrapper still works unchanged** and is not going
away in this release; `rift-lint` flags it with a deprecation hint (`E041`) so you can migrate at
your own pace. A script is v1 if (and only if) it defines a `should_inject` function — v2 scripts
never need to.

```rhai
// v2: named entrypoint
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
// v2: bare-expression form — no `fn respond(ctx) { ... }` wrapper at all. The whole script body
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
| **Lua** | `_rift.script` | High-performance scripting with flow state |

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

> Path parameters (`request.pathParams`, from a stub's [`routePattern`](../configuration/native/#route-patterns-routepattern)) are exposed to the `_rift.script` engines — Rhai, Lua, and JavaScript — not to this Mountebank `inject` `config.request` object.

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

Rhai is a lightweight embedded scripting language optimized for Rust. Scripts must define a `should_inject(request, flow_store)` function.

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
          "code": "fn should_inject(request, flow_store) { let count = flow_store.get(\"demo\", \"counter\"); if count == () { count = 0; }; count += 1; flow_store.set(\"demo\", \"counter\", count); #{inject: true, fault: \"error\", status: 200, body: `{\"count\":${count}}`, headers: #{\"Content-Type\": \"application/json\"}} }"
        }
      }
    }]
  }]
}
```

### Available Variables

```rhai
// Request information
request.method          // String: "GET", "POST", etc.
request.path            // String: "/api/users"
request.headers         // Map: access via request.headers["header-name"]
request.query           // Map: access via request.query["param"]
request.pathParams      // Map: access via request.pathParams["name"] (populated from the stub's routePattern)
request.body            // Parsed JSON body

// Helper functions
timestamp_header()      // RFC 1123 formatted timestamp for HTTP Date header
```

### Flow Store

Flow store provides persistent state across requests. All methods require a `flow_id` parameter to namespace state.

```rhai
// Get value (returns () if not set)
let value = flow_store.get("flow-id", "key");
let count = flow_store.get("flow-id", "counter");
if count == () { count = 0; };

// Set value
flow_store.set("flow-id", "key", "value");
flow_store.set("flow-id", "counter", count + 1);

// Increment counter (returns new value)
let attempts = flow_store.increment("flow-id", "attempts");

// Check existence
if flow_store.exists("flow-id", "key") {
  // key exists
}

// Delete value
flow_store.delete("flow-id", "key");

// Set TTL for entire flow (seconds)
flow_store.set_ttl("flow-id", 300);
```

### Return Values

Scripts must return a map with an `inject` flag:

```rhai
// No injection (pass through to next response or upstream)
#{ inject: false }

// Inject error response
#{
  inject: true,
  fault: "error",
  status: 503,
  body: "{\"error\": \"Service unavailable\"}",
  headers: #{
    "Content-Type": "application/json",
    "Retry-After": "30"
  }
}

// Inject latency
#{
  inject: true,
  fault: "latency",
  duration_ms: 500
}
```

---

## Lua (`_rift.script`)

Lua provides high-performance scripting. Scripts must define a `should_inject(request, flow_store)` function.

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
          "engine": "lua",
          "code": "function should_inject(request, flow_store)\n  local fid = 'lua'\n  local count = flow_store:get(fid, 'count') or 0\n  count = count + 1\n  flow_store:set(fid, 'count', count)\n  return {\n    inject = true,\n    fault = 'error',\n    status = 200,\n    body = '{\"count\":' .. count .. '}',\n    headers = {['Content-Type'] = 'application/json'}\n  }\nend"
        }
      }
    }]
  }]
}
```

### Available Variables

```lua
-- Request information (passed as first argument)
request.method          -- String
request.path            -- String
request.headers         -- Table: request.headers["header-name"]
request.query           -- Table: request.query["param"]
request.pathParams      -- Table: request.pathParams["name"]
request.body            -- Parsed body (table or string)

-- Standard Lua functions
math.random()           -- Float 0.0 to 1.0
math.random(n)          -- Integer 1 to n
math.random(m, n)       -- Integer m to n
os.time()               -- Unix timestamp
os.date("*t")           -- Date table
```

### Flow Store

Lua uses colon syntax for method calls:

```lua
-- Get value (returns nil if not set)
local value = flow_store:get("flow-id", "key")
local count = flow_store:get("flow-id", "counter") or 0

-- Set value
flow_store:set("flow-id", "key", "value")
flow_store:set("flow-id", "counter", count + 1)

-- Increment counter (returns new value)
local attempts = flow_store:increment("flow-id", "attempts")

-- Check existence
if flow_store:exists("flow-id", "key") then
  -- key exists
end

-- Delete value
flow_store:delete("flow-id", "key")

-- Set TTL for entire flow (seconds)
flow_store:set_ttl("flow-id", 300)
```

### Return Values

```lua
-- No injection
return { inject = false }

-- Inject error response
return {
  inject = true,
  fault = "error",
  status = 503,
  body = '{"error": "Service unavailable"}',
  headers = {
    ["Content-Type"] = "application/json",
    ["Retry-After"] = "30"
  }
}

-- Inject latency
return {
  inject = true,
  fault = "latency",
  duration_ms = 500
}
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
          "code": "fn should_inject(request, flow_store) { let fid = \"ratelimit\"; let count = flow_store.get(fid, \"requests\"); if count == () { count = 0; }; count += 1; flow_store.set(fid, \"requests\", count); if count > 100 { #{inject: true, fault: \"error\", status: 429, body: `{\"error\":\"Rate limit exceeded\",\"count\":${count}}`, headers: #{\"Content-Type\": \"application/json\", \"Retry-After\": \"60\"}} } else { #{inject: false} } }"
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
    "flowState": {"backend": "inmemory", "ttlSeconds": 300}
  },
  "stubs": [{
    "responses": [{
      "_rift": {
        "script": {
          "engine": "rhai",
          "code": "fn should_inject(request, flow_store) { let flow_id = request.headers[\"x-flow-id\"]; if flow_id == () { flow_id = \"default\"; }; let attempts = flow_store.get(flow_id, \"attempts\"); if attempts == () { attempts = 0; }; attempts += 1; flow_store.set(flow_id, \"attempts\", attempts); if attempts <= 2 { #{inject: true, fault: \"error\", status: 503, body: `{\"error\":\"Temporary failure\",\"attempt\":${attempts}}`, headers: #{\"Content-Type\": \"application/json\"}} } else { #{inject: false} } }"
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
    flowState: { backend: inmemory, ttlSeconds: 300 }
  stubs:
    - responses:
        - _rift:
            script:
              engine: rhai
              # Block scalar: the exact retry logic from the JSON example above, just readable.
              code: |
                fn should_inject(request, flow_store) {
                  let flow_id = request.headers["x-flow-id"];
                  if flow_id == () { flow_id = "default"; }
                  let attempts = flow_store.get(flow_id, "attempts");
                  if attempts == () { attempts = 0; }
                  attempts += 1;
                  flow_store.set(flow_id, "attempts", attempts);
                  if attempts <= 2 {
                    #{
                      inject: true, fault: "error", status: 503,
                      body: `{"error":"Temporary failure","attempt":${attempts}}`,
                      headers: #{"Content-Type": "application/json"}
                    }
                  } else {
                    #{ inject: false }
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
    # Named registry (issue #356): give a script a name once, `ref:` it from any response.
    scripts:
      failTwice:
        file: scripts/fail-twice.rhai   # engine inferred from the extension: .rhai -> rhai
  stubs:
    - responses:
        - _rift:
            script:
              ref: failTwice
```

`engine` is inferred from `file`'s extension (`.rhai` -> `rhai`, `.lua` -> `lua`, `.js` ->
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
            "code": "fn should_inject(request, flow_store) { let fid = \"demo\"; let counter = flow_store.get(fid, \"counter\"); if counter == () { counter = 0; }; counter += 1; flow_store.set(fid, \"counter\", counter); #{inject: true, fault: \"error\", status: 200, body: `{\"counter\":${counter}}`, headers: #{\"Content-Type\": \"application/json\"}} }"
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
            "code": "fn should_inject(request, flow_store) { let fid = \"demo\"; let counter = flow_store.get(fid, \"counter\"); if counter == () { counter = 0; }; #{inject: true, fault: \"error\", status: 200, body: `{\"counter\":${counter}}`, headers: #{\"Content-Type\": \"application/json\"}} }"
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
            "code": "fn should_inject(request, flow_store) { let fid = \"demo\"; flow_store.delete(fid, \"counter\"); #{inject: true, fault: \"error\", status: 200, body: \"{\\\"message\\\":\\\"Counter reset\\\"}\", headers: #{\"Content-Type\": \"application/json\"}} }"
          }
        }
      }]
    }
  ]
}
```

---

## `ctx` API v2

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

(Full `get_or`/parameterized `incr`/CAS support is a separate, upcoming addition — this covers the
common get/set/incr/exists/delete case today.)

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

Replace the v1 `#{ inject:, fault: }` map. Available in `respond(ctx)`/`transform(ctx)` (and as the
return value of a bare-expression script for those placements):

| Constructor | Meaning | Replaces (v1) |
|:------------|:--------|:---------------|
| `http(status)` / `http(status, body)` | respond with this status/body; chain `.header(k, v)` for extra headers | `#{inject:true, fault:"error", status, body, headers}` |
| `delay(ms)` | inject latency, then respond normally | `#{inject:true, fault:"latency", duration_ms}` |
| `reset()` | reset the connection (transport-level) | `fault: "tcp"` / a top-level `fault` string |
| `pass()` | respond normally, no injection | `#{inject: false}` |
| *(nothing)* | same as `pass()` | `#{inject: false}` |

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
  configured, otherwise **5000 ms**. Rhai and Lua are interrupted mid-run when the deadline passes.
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
itself omits `engine`: `"rhai"` (default), `"lua"`, or `"javascript"`. A per-script `engine` field
always takes precedence over `defaultEngine`.

```json
{
  "_rift": {
    "scriptEngine": { "defaultEngine": "lua" }
  }
}
```

## Flow-Store Error Semantics

By **default** a flow-store backend failure (e.g. a Redis outage mid-request) is **lenient**: the
failing op returns its empty fallback (`()` / `nil` / `null`, `false`, or `0`) and the script keeps
running. So a script can't tell "key genuinely absent" from "backend down" by the return value
alone — use `last_error()` for that:

```rhai
let v = flow_store.get("flow-1", "attempts");
if flow_store.last_error() != () {
  // the backend failed on the last op — v is a fallback, not real data
}
```

`last_error()` returns the most recent backend error (or `()` / `nil` / `null` when the last op
succeeded) and is reset at the start of every script execution. The call syntax follows each engine:
`flow_store.last_error()` (Rhai), `flow_store:last_error()` (Lua), `flow_store.last_error()` (JS).

Set the **`RIFT_STRICT_FLOW_STORE`** environment variable (truthy: `1`/`true`/`yes`/`on`) to make a
flow-store op failure **raise** a script error in all three engines instead of returning a fallback.
The raised error propagates to the standard script-error path (`500` with `x-rift-script-error`).

---

## Engine Comparison

| Feature | JavaScript | Rhai | Lua |
|:--------|:-----------|:-----|:----|
| Format | `inject` response | `_rift.script` | `_rift.script` |
| State access | `state.key` | `flow_store.get(id, key)` | `flow_store:get(id, key)` |
| Flow isolation | Per imposter | Per flow_id | Per flow_id |
| Function wrapper | None needed | `respond(ctx)`/bare (v2, recommended) or `should_inject(request, flow_store)` (v1, deprecated) | same as Rhai |
| Performance | Good | Excellent | Excellent |
| Mountebank compatible | Yes | No | No |

---

## Performance Tips

1. **Use Rhai/Lua for high-throughput** - Both are compiled and cached for efficient reuse
2. **Minimize flow store access** - Each get/set has overhead; batch operations when possible
3. **Keep scripts simple** - Complex logic is harder to debug and maintain
4. **Use flow_id wisely** - Namespace state by request ID, user ID, or session to avoid collisions
5. **Set appropriate TTLs** - Prevent unbounded state growth with `ttlSeconds` config
