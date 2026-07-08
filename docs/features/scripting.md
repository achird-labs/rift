---
layout: default
title: Scripting
parent: Features
nav_order: 2
---

# Scripting

Rift supports multiple scripting engines for dynamic behavior.

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
| Function wrapper | None needed | `should_inject(request, flow_store)` | `should_inject(request, flow_store)` |
| Performance | Good | Excellent | Excellent |
| Mountebank compatible | Yes | No | No |

---

## Performance Tips

1. **Use Rhai/Lua for high-throughput** - Both are compiled and cached for efficient reuse
2. **Minimize flow store access** - Each get/set has overhead; batch operations when possible
3. **Keep scripts simple** - Complex logic is harder to debug and maintain
4. **Use flow_id wisely** - Namespace state by request ID, user ID, or session to avoid collisions
5. **Set appropriate TTLs** - Prevent unbounded state growth with `ttlSeconds` config
