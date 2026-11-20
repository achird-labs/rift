---
title: WebAssembly scripting
---

# WebAssembly scripting

A `_rift.script` with `"engine": "wasm"` runs a WebAssembly **component** instead of a Rhai or
JavaScript script. The component can be written in any language that builds for
`wasm32-wasip2`. It runs in a sandbox under [wasmtime](https://wasmtime.dev), with limits that stop
it mid-run: a wall-clock deadline, an optional fuel budget, and a memory cap.

The `rift` binary includes this engine, and admits components by default (`--allowWasm`, see the
[security model](#security-model)). An embedder linking `librift_ffi` needs the library's
`wasm` feature.

---

## A first component

[`examples/wasm/counter-twin`](https://github.com/achird-labs/rift/tree/master/examples/wasm/counter-twin)
is a complete Rust component. Each request increments `count` in the imposter's flow store, and
answers `409` once the count is above `knobs.limit`:

```sh
rustup target add wasm32-wasip2
cd examples/wasm/counter-twin
cargo build --release --target wasm32-wasip2
rift --allowInjection --configfile imposter.json
```

<!-- rift-lint: skip -->
```json
{
  "_rift": {
    "script": {
      "file": "target/wasm32-wasip2/release/counter_twin.wasm",
      "knobs": { "limit": 2 }
    }
  }
}
```

The `.wasm` extension selects the engine, so `"engine": "wasm"` can be left out.

---

## Configuration

| Key | Default | Meaning |
|:----|:--------|:--------|
| `_rift.script.engine` | from the file extension | `"wasm"`. |
| `_rift.script.file` | — | The component. It is resolved like any script file: relative to the config file for `--configfile`, or to `--scripts-dir` through the admin API. A path that leaves `--scripts-dir` is refused. |
| `_rift.script.export` | `"rift"` | Which export answers. `"rift"` is the `rift:mock/handler` interface below. It is the only value for now. |
| `_rift.script.knobs` | `{}` | Any JSON. The component's `init` gets it as text before every call. |
| `_rift.scriptEngine.timeoutMs` | `5000` | The wall-clock deadline of one call. |
| `_rift.scriptEngine.wasm.fuel` | unlimited | A compute budget in wasmtime fuel units. |
| `_rift.scriptEngine.wasm.maxMemoryMiB` | `64` | The most linear memory one call may use, all of its memories together. |
| `_rift.scriptEngine.wasm.maxTables` | `4` | The most tables one call may create. |
| `_rift.scriptEngine.wasm.maxTableElements` | `100000` | The most elements one table may hold. |

**The component travels in the imposter.** A component has no inline source. When an imposter is
created, Rift reads the `.wasm` file and stores its bytes in `_rift.script.code` as base64, as it
stores a Rhai file's text. `GET /imposters`, `mb save` and `--datadir` therefore hold the
component itself, and replay without the file. A `code` that is not base64 is refused with
`engine "wasm" has no inline form; use file` (rift-lint reports it as `E062`).

The component is compiled once, when the imposter is created, so a broken component is refused
with `400` before any request reaches it. Compiled components are cached by their content: two
imposters carrying the same bytes compile once.

---

## The `rift:mock` world

A component implements this world. The WIT file is
[`crates/rift-mock-core/wit/rift-mock.wit`](https://github.com/achird-labs/rift/blob/master/crates/rift-mock-core/wit/rift-mock.wit).

```wit
package rift:mock@0.1.0;

interface types {
  record header { name: string, value: string }
  record request {
    method: string, path: string,
    query: list<tuple<string, string>>, path-params: list<tuple<string, string>>,
    headers: list<header>, body: list<u8>,
    flow-id: string, port: u16,
    stub-id: option<string>, scenario-name: option<string>, scenario-state: option<string>,
  }
  record response { status: u16, headers: list<header>, body: list<u8> }
  variant decision { pass, delay(u64), reset, respond(response) }
}

interface state {
  variant cas-outcome { applied, conflict(option<string>) }
  get:     func(key: string, flow: option<string>) -> result<option<string>, string>;
  set:     func(key: string, value: string, flow: option<string>) -> result<_, string>;
  incr-by: func(key: string, by: s64, flow: option<string>) -> result<s64, string>;
  exists:  func(key: string, flow: option<string>) -> result<bool, string>;
  delete:  func(key: string, flow: option<string>) -> result<_, string>;
  cas:     func(key: string, expected: option<string>, new: string, flow: option<string>) -> result<cas-outcome, string>;
  ttl:     func(seconds: s64, flow: option<string>) -> result<_, string>;
  key-ttl: func(key: string, seconds: s64, flow: option<string>) -> result<bool, string>;
  clear:   func(flow: option<string>) -> result<_, string>;
}

interface handler {
  use types.{request, decision};
  init:    func(knobs: string) -> result<_, string>;
  respond: func(req: request) -> result<decision, string>;
}

world mock {
  import state;
  export handler;
}
```

- **`respond`** answers each request. Its `decision` is what a Rhai or JavaScript script returns:
  `pass` serves the response normally, `delay(ms)` adds latency, `reset` resets the connection,
  and `respond(response)` answers with that status, headers and body. The body must be UTF-8.
  An `err(message)` is a script error.
- **`init`** runs before `respond`, with `knobs` as JSON text.
- **`state`** is the imposter's [flow store](flow-state.md), shared with every other script on the
  imposter. `flow = none` is the request's flow id, the one `ctx.state` uses; `some(id)` is another
  flow. Values are JSON text; TTLs are seconds. A failing store returns `err`, never a default.

Every request gets a fresh instance: nothing in the component's memory carries from one request to
the next. State that should last goes in `state`.

A component may export more than `handler`, and it may import the WASI interfaces below.

### Toolchains

Anything that produces a `wasm32-wasip2` component for this world works:

| Language | Tool |
|:---------|:-----|
| Rust | [`wit-bindgen`](https://github.com/bytecodealliance/wit-bindgen) and `cargo build --target wasm32-wasip2` |
| JavaScript | [`componentize-js`](https://github.com/bytecodealliance/ComponentizeJS) |
| Python | [`componentize-py`](https://github.com/bytecodealliance/componentize-py) |
| Go | [TinyGo](https://tinygo.org) with `-target=wasip2` |

A core WebAssembly module, built for `wasm32-unknown-unknown` or `wasm32-wasip1`, is refused with
`not a component; build for wasm32-wasip2` (rift-lint reports it as `E063`). A precompiled
`.cwasm` file is not accepted. The wasm GC and memory64 proposals are off.

---

## Limits

| Limit | Set by | When it is hit |
|:------|:-------|:---------------|
| Deadline | `timeoutMs` | `504` with `x-rift-script-timeout: true`, as for Rhai and JavaScript. |
| Fuel | `wasm.fuel` | `500`, `Script error: fuel exhausted (<fuel> units)`. |
| Memory | `wasm.maxMemoryMiB` | `500`, `Script error: memory limit exceeded (maxMemoryMiB <n>): …`. The cap counts every linear memory the call creates. |
| Any other trap | — | `500`, `Script error: wasm trap: …`. |
| Guest error | `respond` returns `err` | `500`, `Script error: <message>`. |

Every failure carries `x-rift-script-error: true` and is counted in `rift_script_errors_total`, and
`rift_wasm_traps_total{kind}` counts it by kind (`deadline`, `fuel`, `memory`, `trap`,
`guest-error`).

The deadline interrupts a component even in a loop that never returns. A running component also
gives up its thread every 10 ms, so a slow component does not hold up other requests. Fuel is not a
time limit: it counts instructions, so the same component on the same input always spends the same
fuel. Use it when a test needs a budget that does not depend on the machine.

---

## Logs

Whatever a component writes to stdout or stderr is logged like a Rhai or JavaScript `ctx.logger`
line: stdout at info, stderr at warn, tagged `engine=wasm` and the first 12 hex digits of the
component's SHA-256. Under [debug mode](debug-mode.md) the lines appear in the
`x-rift-script-trace` header, with `fuelUsed` and, after a failure, `trap`. Each stream keeps its
first 64 KiB per request.

`rift script run counter_twin.wasm --state count=1` runs a component once against a request
fixture, with `{}` as its knobs and the default limits, and `rift script check counter_twin.wasm`
compiles and links it without running it. `rift script check imposter.json` also checks each
component's `export`.

---

## Security model

The sandbox works by not providing things. A `wasm32-wasip2` toolchain makes every component import
a set of standard WASI interfaces even when the program uses none of them, so refusing imports
would refuse real components. Instead, Rift links each one with nothing behind it:

| Interface | Under Rift |
|:----------|:-----------|
| `wasi:cli` (environment, arguments, exit, std streams, terminal) | No environment variables, no arguments. stdout and stderr are captured as logs; stdin is empty. |
| `wasi:filesystem` | No directories are opened to the component, so it sees no files. |
| `wasi:clocks` | The host's wall and monotonic clocks. |
| `wasi:random` | The host's random source. |
| `wasi:io` | Streams for the interfaces above. |
| `rift:mock/state` | The imposter's flow store. |
| `wasi:sockets`, `wasi:http` | **Not linked.** A component importing them is refused at creation with `wasm component imports <interface>, which Rift does not provide`. |

What this does **not** isolate:

- **CPU.** A running component can use its thread until the next 10 ms tick, and for the whole
  `timeoutMs` deadline. Compiling a component, when an imposter is created or the first time a
  restarted engine serves it, also uses a thread.
- **Host memory for compiled code.** `maxMemoryMiB` caps the component's own memory, not the
  memory Rift spends compiling and caching it. A call may hold at most 1024 host resources
  (streams, pollables) at once.
- **The flow store.** A component reads and writes the same store as the imposter's other scripts
  and `stateOps`.

### `--allowWasm`

Because a component reaches nothing on the host, it is admitted by `--allowWasm` (on by default),
not by `--allowInjection`: an operator does not have to allow host code for everyone to let anyone
load a component. `--allow-wasm false` (rcfile `allowWasm`, env `RIFT_ALLOW_WASM`, FFI
`allowWasm`) refuses components at every door that checks `--allowInjection`: `400 invalid wasm`
on the admin API and on `POST /admin/reload`, a startup error for `--configfile`, and a skipped
file for `--datadir`. A document that also carries host code (an `inject`, a Rhai script) still
needs `--allowInjection` for that part.

What the flag promises is the table above, no more: it admits code whose only reach is the
interfaces Rift links. It does not make a component's CPU time, compile time or flow-store writes
free, and the limits are what bound those. `GET /config` reports it as `options.allowWasm`, and
`features` says whether the build has the engine at all.
