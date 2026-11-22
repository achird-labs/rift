---
title: The rift:mock WIT contract
---

# The `rift:mock` WIT contract

`rift:mock` is the world a `_rift.script` with `engine: "wasm"` runs ([WebAssembly
scripting](wasm-scripting.md)). It is a public contract. Guest authors build against it without
cloning Rift. Other hosts implement it, so the same component runs there and behind an imposter.
This page specifies every function. It also states the compatibility rule for changes.

- **Source:** [`crates/rift-mock-core/wit/rift-mock.wit`](https://github.com/achird-labs/rift/blob/master/crates/rift-mock-core/wit/rift-mock.wit).
  The file stays inside the crate, because `rift-mock-core` is published to crates.io and its
  bindings read the file from there.
- **Release asset:** each release attaches `rift-mock-wit-<version>.tar.gz` (with a `.sha256`). It
  contains the WIT, the conformance guest's source and script, and the guest built for
  `wasm32-wasip2`.
- **Current version:** `rift:mock@0.1.0`.

## The world

```wit
world mock {
  import state;
  export handler;
}
```

A component imports `state` and exports `handler`. It may export more than that. It may also import
the WASI interfaces a `wasm32-wasip2` toolchain emits: `wasi:cli`, `wasi:io`, `wasi:filesystem`,
`wasi:clocks` and `wasi:random`. See [What a host links](#what-a-host-links).

## `state`: the imposter's flow store

Every function takes `flow: option<string>`. `none` is the request's own flow, the one
`request.flow-id` names and `ctx.state` is bound to in Rhai and JavaScript. `some(id)` is another
flow, as `ctx.store.flow(id)` is.

**Values are JSON text.** A host stores the JSON value, not the text. So `get` returns text that is
*equal as JSON* to what was stored, not necessarily the same bytes. Rift returns it compact (object
keys currently come out sorted); a guest must not rely on the exact bytes. A value written as text that is not JSON is refused, and nothing is stored.

**Errors** are `"<op>: <detail>"` strings, where `<op>` is the name below for the function. The
prefix is stable; the detail is the backend's own wording. Errors with a stable message:

| Error | Message starts with |
|:------|:--------------------|
| A value, `expected` or `new` that is not JSON | `<op>: value is not JSON` |
| `incr-by` past the range of `s64` | `incrementBy: increment_by overflow` |

Rift's in-memory store does not fail otherwise. A store an embedder plugs in through a
`FlowStoreProvider` can fail on any call, with `"<op>: <its error>"`.

| Function | `<op>` | Result |
|:---------|:-------|:-------|
| `get(key, flow)` | `get` | `some(text)` if the key is present, `none` if it is missing or expired. JSON `null` is a value: `some("null")`. |
| `set(key, value, flow)` | `set` | Stores `value`. |
| `incr-by(key, by, flow)` | `incrementBy` | Adds `by` (which may be negative) and returns the new value, atomically. A missing key starts at 0. **A value that is not an integer** (a string, an object, `1.5`) **counts as 0 and is overwritten.** On overflow the call fails and the value is left as it was. |
| `exists(key, flow)` | `exists` | Whether the key is present (and not expired). |
| `delete(key, flow)` | `delete` | Removes the key. Deleting a missing key succeeds. |
| `cas(key, expected, new, flow)` | `cas` | Atomically sets `new` if the current value equals `expected`, compared as JSON. `expected = none` means "the key is absent". Returns `applied`, or `conflict(current)` where `current` is the value that won (`none` if the key is absent). |
| `ttl(seconds, flow)` | `setTtl` | Gives every key the flow holds now a lifetime of `seconds` from now. `seconds <= 0` drops every key of the flow. Keys written afterwards get the default lifetime. |
| `key-ttl(key, seconds, flow)` | `setKeyTtl` | Gives one key a lifetime of `seconds` from now and returns `true`, or `false` if the key is missing. `seconds <= 0` deletes the key and returns whether it existed. |
| `clear(flow)` | `clear` | Removes every key of the flow. Clearing an empty flow succeeds. |

**Lifetimes.** TTLs are whole seconds. A key that is past its lifetime reads as absent everywhere
(`get`, `exists`, `cas`, `key-ttl`), whether or not it has been swept yet. Every write (`set`, an
`incr-by`, a `cas` that applies) gives the key the imposter's default lifetime
(`_rift.flowState.ttlSeconds`, 300 seconds unless set). That replaces any lifetime `key-ttl` or
`ttl` gave it before. Lifetimes are measured on the imposter's clock, which a test can freeze or
step ([`/imposters/{port}/clock`](../api/index.md#imposter-clock)).

**`request.flow-id`** is the request's flow. `_rift.flowState.flowIdSource` decides it:

- `"imposter_port"` (the default) uses the imposter's port as text.
- `"header:<Name>"` uses that request header, matched case-insensitively, taking the first value
  when it repeats. If the header is missing, the port is used.

## `handler`: what a component exports

| Function | Called | Result |
|:---------|:-------|:-------|
| `init(knobs)` | Once per instantiation, before `respond`. `knobs` is `_rift.script.knobs` as JSON text, `{}` when absent. | `err(msg)` fails the request: `500`, `Script error: init: <msg>`. |
| `respond(req)` | Once per request. | A `decision`, or `err(msg)`: `500`, `Script error: <msg>`. |

With the default `instance: "request"`, Rift instantiates a component **for every request**, so
`init` runs on every request and nothing in the component's memory survives between requests: state
that must persist goes through `state`. With `instance: "imposter"` one instance is kept and `init`
runs once, when it is made (see [Keeping an
instance](wasm-scripting.md#keeping-an-instance-instance-imposter)).

The records and variants `handler` uses (`header`, `request`, `response`, `decision`) are defined
in the `types` interface, which `state` and `handler` share.

`request` fields:

- `method` and `path` are as received.
- `query` holds the query parameters, one pair per value. Names come in the order each first
  appeared; a repeated name's values are adjacent, in the order received (`?a=1&b=2&a=3` arrives
  as `a=1, a=3, b=2`).
- `path-params` holds the matched route's parameters, sorted by name.
- `headers` holds the headers, one entry per value, ordered the same way as `query`.
- `body` is the raw request body. A binary-mode request body arrives decoded, not as base64.
- `flow-id` is the request's flow, as above. `port` is the imposter's port.
- `stub-id`, `scenario-name` and `scenario-state` describe the stub that matched, when it has them.

`decision` cases: `pass` answers with the stub's own response. `delay(ms)` adds latency in
milliseconds. `reset` resets the TCP connection. `respond(response)` answers with that status,
those headers and that body. Rhai and JavaScript spell these `pass()`, `delay(ms)`, `reset()` and
`http(status, body)`. Through `handler`, a `respond` body must be UTF-8; otherwise the request fails
with `500`.

## `introspect`: an optional state reader

A component kept with `instance: "imposter"` may also export `introspect`, beside `handler` or
`wasi:http/incoming-handler`. It is not part of `world mock`: a component without it runs as before.

| Function | Called | Result |
|:---------|:-------|:-------|
| `snapshot()` | By `GET /imposters/:port/wasm/snapshot`, never by a request. | JSON text describing the component's observable state, answered byte for byte; `err(msg)` answers `500` with `msg`, and text that is not JSON `500 snapshot is not JSON`. |

During `snapshot` the `state` writes return `err("read-only during snapshot")` and `wasi:random`
draws from a throwaway stream, so a snapshot does not change the run it reads. A guest must not
change its own memory in `snapshot` either: nothing enforces that, and one that does breaks a seeded
replay. See [Reading a twin's state](wasm-scripting.md#reading-a-twins-state-riftmockintrospect).

## What a host links

A component built with a standard toolchain imports WASI interfaces whether or not its code uses
them. A host must link them so the component instantiates, with nothing behind them that reaches
the host:

| Interface | Rift links it as |
|:----------|:-----------------|
| `wasi:cli` (environment, exit, std streams, terminal) | No environment, no arguments, no stdin. stdout and stderr are captured as the script's log. |
| `wasi:filesystem` | No preopened directory. |
| `wasi:io` | As needed by the others. |
| `wasi:clocks` | **Virtualised:** the imposter's clock — the system clock unless a test froze or stepped it — or a request's `X-Rift-Now` ([Time](wasm-scripting.md#time)). |
| `wasi:random` | **Virtualised:** the imposter's random stream, seeded by `_rift.scriptEngine.seed` or from the system's entropy ([Randomness](wasm-scripting.md#randomness)). |
| `wasi:http/types` | For a component Rift serves with `export: "http"` ([The `http` export](wasm-scripting.md#the-http-export)); types only, no client. |

A host must **not** link `wasi:sockets` or `wasi:http/outgoing-handler`. A component that imports
either is refused when it is admitted, so a component cannot reach the network from inside a mock.
A host that drives simulated time should back `wasi:clocks` and, for repeatable runs,
`wasi:random` with its own sources, as Rift does.

## Conformance

`crates/rift-mock-core/wit/conformance/` holds a guest and `script.json`, a list of `state` calls
with the result each must give. The release asset carries both, plus the guest already built.

To check a host:

1. Instantiate `rift_mock_conformance.wasm` on a store that has no state for the request's flow or
   for the flow `conformance-other`.
2. Call `init` with the text of `script.json`, then `respond` once.
3. Read the decision. `respond` with status `200` means every call gave the specified result.
   `422` carries a JSON report listing each mismatch: its step, the call, and what was expected
   and returned.

Rift runs the same script against its in-memory store in CI, in
`crates/rift-mock-core/tests/wit_contract.rs`. That test is ignored by default because it needs the
guest built. It reads the guest from `wit/conformance/target/wasm32-wasip2/release/`, or from the
path in `RIFT_CONFORMANCE_COMPONENT`:

```bash
cargo build --release --target wasm32-wasip2 --manifest-path crates/rift-mock-core/wit/conformance/Cargo.toml
cargo test -p rift-mock-core --features wasm --test wit_contract -- --include-ignored
```

## Compatibility

Wasmtime, Rift's host, treats `0.1.x` versions of a package as one compatible range in both
directions. A guest that imports `state@0.1.1` also resolves against a host offering `state@0.1.0`.
So an interface, once published, never changes. The rule is:

- **Additive** changes bump the patch version: `0.1.0` → `0.1.1`. These are a new interface or a
  new optional export. An existing interface is never altered, not even by adding a function to it.
  A new function goes in a new interface.
- **Breaking** changes bump the minor version: `0.2.0`. These are a changed signature, record field
  or variant case. Rift keeps linking `0.1.x` for one release after a `0.2.0` ships.
- **`1.0.0`** is the point at which the compatible range moves to the major version, as for any
  semver package.

Rift links the newest `0.1.x` it knows, and a guest built against an older one keeps working.
