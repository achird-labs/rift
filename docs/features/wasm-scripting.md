---
title: WebAssembly scripting
---

# WebAssembly scripting

A `_rift.script` with `"engine": "wasm"` runs a WebAssembly **component** instead of a Rhai or
JavaScript script. The component can be written in any language that builds for
`wasm32-wasip2`. It runs in a sandbox under [wasmtime](https://wasmtime.dev), with limits that stop
it mid-run: a wall-clock deadline, an optional fuel budget, and a memory cap.

The released `rift` binary and images include this engine and admit components by default
(`--allowWasm`, see the [security model](#security-model)). The C-ABI library `librift_ffi` does
**not** by default: the engine adds about 13 MB, so an embedder builds the library with
`--features wasm`. See [Build features](../embedding/ffi.md#build-features).

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
| `_rift.script.component` | — | Instead of `file`: a component uploaded with `POST /components`, as `"sha256:<hex>"` ([Uploading a component](#uploading-a-component)). Implies `engine: "wasm"`. |
| `_rift.script.export` | `"rift"` when the component exports `rift:mock/handler`, else `"http"` | Which export answers: `"rift"` is the `rift:mock/handler` interface below, `"http"` a standard `wasi:http/incoming-handler` ([The `http` export](#the-http-export)). Naming one the component does not export is refused with `400`. |
| `_rift.script.instance` | `"request"` | `"request"` runs a fresh instance for every call; `"imposter"` keeps one instance across calls, so the guest's memory is its state ([Keeping an instance](#keeping-an-instance-instance-imposter)). |
| `_rift.script.knobs` | `{}` | Any JSON. Under `"rift"`, the component's `init` gets it as text before every call; under `"http"`, the `RIFT_KNOBS` environment variable holds it. |
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

### Uploading a component

A test or an agent that has just built a `.wasm` can hand it to a running Rift, local or remote,
without putting a file on its host:

```bash
sha=$(curl -s -X POST localhost:2525/components \
  -H 'Content-Type: application/wasm' --data-binary @counter_twin.wasm | jq -r .sha256)

curl -X POST localhost:2525/imposters -H 'Content-Type: application/json' -d '{
  "port": 4545, "protocol": "http",
  "stubs": [{ "responses": [{ "_rift": { "script": { "component": "sha256:'"$sha"'" } } }] }]
}'
```

The upload is validated as a `file` component is, and refused with `400` naming the problem. The
same bytes again answer `200` with the same hash. A stub that names a hash the server does not
hold is refused at the admin API with `400` (`unknown component sha256:…; upload it with POST
/components`).

**Pinning by hash.** A reference names the exact bytes that run. To run new code, upload it and
point the stub at the new hash (`PUT /imposters/{port}/stubs`, or a recreate). Requests already
running finish on the old component, and new requests use the new one. Every call has its own
store, so nothing is swapped mid-request.

**Where components live.** Without `--datadir` the registry is in memory and empty after a
restart. With `--datadir`, each upload is also written to `<datadir>/components/<sha256>.wasm`
and loaded at startup, its hash re-checked; a file whose bytes no longer match its name is skipped
and reported. A config loaded from `--configfile` or `--datadir` that names a component the server
does not hold still loads, and that stub answers `500` with `Script error: unknown component
sha256:…` until the component is uploaded. The registry belongs to one server; it is not shared
between servers.

**`--strict-wasm <file>`** limits which components run at all. The file lists one lowercase
SHA-256 per line (`#` starts a comment). With it set, a component whose hash is not listed is
refused when it is uploaded and wherever a stub carrying it is admitted, whether it came in by
`component`, `file` or `code`: `component sha256:… is not in --strict-wasm`. The trust model is
to approve the behaviour, keyed by the hash: you never have to judge whether the code is safe to
run. The list is read at startup; a change needs a restart. Env `RIFT_STRICT_WASM`, rcfile
`strictWasm`.

### Earning trust

A component that imitates a real service is a hypothesis until it is checked against that service.
Rift already has the evidence: record the real service through a [proxy](../mountebank/proxy.md),
save the imposter, and replay the recording against the component with
[`rift wasm check`](../configuration/cli.md#wasm):

```bash
rift wasm check --component twin.wasm --corpus saved.json
```

Each recorded request is sent to the component in recorded order, on the recorded clock, and every
answer that differs is reported by stub and field. Report the result for what it is: **inside the
corpus, the component matches the real service; outside it, it is a hypothesis.** A clean check over
three recorded calls says nothing about a fourth kind of request. Grow the recording where the
component will be relied on, and rerun the check whenever the component or the recording changes
(`--json` gives CI and agents a report to gate on).

---

## The `rift:mock` world

A component implements this world. The WIT file is
[`crates/rift-mock-core/wit/rift-mock.wit`](https://github.com/achird-labs/rift/blob/master/crates/rift-mock-core/wit/rift-mock.wit).
Every function's semantics, the compatibility policy for changes and a conformance guest for other
hosts are in [The `rift:mock` WIT contract](wasm-wit.md).

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
  and `respond(response)` answers with that status, headers and body. The body must be UTF-8;
  a binary body needs [the `http` export](#the-http-export). An `err(message)` is a script error.
- **`init`** runs before `respond`, with `knobs` as JSON text.
- **`state`** is the imposter's [flow store](flow-state.md), shared with every other script on the
  imposter. `flow = none` is the request's flow id, the one `ctx.state` uses; `some(id)` is another
  flow. Values are JSON text; TTLs are seconds. A failing store returns `err`, never a default.

By default every request gets a fresh instance: nothing in the component's memory carries from one
request to the next, and state that should last goes in `state`. To keep the guest's memory, see
[Keeping an instance](#keeping-an-instance-instance-imposter).

A component may export more than `handler`, and it may import the WASI interfaces below.

### The `http` export

`"export": "http"` serves a standard `wasi:http/proxy` component: one that exports
`wasi:http/incoming-handler`, as `componentize-js`, `componentize-py`, TinyGo and Spin-style Rust
(the [`wasip2`](https://crates.io/crates/wasip2) crate's `http::proxy::export!`) build. The
component answers the whole response:

- **The request** is the one the stub matched: method, path, the query exactly as the client sent
  it, every header value, the body's bytes, and `https` as the scheme on an `https` imposter. A
  request without `Host` (an HTTP/2 one) gets the imposter's address, since `wasi:http` needs an
  authority.
- **The response** is served as the component sets it: status, every header value (two
  `set-cookie`s stay two), and the body's bytes, which need not be UTF-8. Rift adds
  `x-rift-imposter` and `x-rift-script: wasm`, and frames the body itself, so `content-length`,
  `transfer-encoding` and `connection` from the component are dropped. A `content-length` the
  component sets must match its body: a body cut short (a component that returns without finishing
  it) is a script error, not a shorter `200`. Trailers passed to `outgoing-body.finish` are not
  sent.
- **Knobs** arrive in the `RIFT_KNOBS` environment variable, as JSON text (`{}` when absent).
  `wasi:http` has no `init`, and this is the only environment variable a component ever sees.
- **The body is read while `handle` runs**, so a component may set the response and keep writing
  its body. A body past `maxMemoryMiB` is a script error, not a truncated `200`.
- **Failures are script errors** (`500`): a component that returns without setting its
  `response-outparam` (`component returned no response`), one that sets it to an error, a body
  that fails, and traps, fuel, memory and deadline misses as under `"rift"`.
- **There is no `delay` or `reset`**: an http component can only answer. `_rift.fault` and
  behaviors still apply around the stub.
- **Outgoing HTTP is never available.** `wasi:http/types` is linked; `wasi:http/outgoing-handler`
  is not, so a component importing it is refused when it is admitted.
- **Host memory.** The bytes a `wasi:http` body queues, and its header maps, live in Rift's memory,
  not the component's, so `maxMemoryMiB` does not count them. They are bounded: a body queues at
  most one 64 KiB write, a header map holds at most 32 KiB, and a call holds at most 1024
  resources, so one call can hold at most about 64 MiB this way.
- It may import `rift:mock/state` too, for the imposter's flow store.

A component may export both `rift:mock/handler` and `wasi:http/incoming-handler`. Without
`export`, Rift runs `rift`; name `"http"` to serve the other. The example
[`examples/wasm/http-twin`](https://github.com/achird-labs/rift/tree/master/examples/wasm/http-twin)
is one, so a simulation runtime can drive it directly while Rift serves it over HTTP.

### Keeping an instance: `instance: "imposter"`

`"instance": "imposter"` keeps one live instance of the component on the imposter and calls it for
every request, so state the guest keeps in its own memory lasts. This is the mode for a twin that a
simulation runtime also drives directly, and one call costs about 3 µs instead of 21–25 µs, since
nothing is instantiated. It works with either export; `init(knobs)` runs once, when the instance is
made.

| | `"request"` (default) | `"imposter"` |
|:--|:--|:--|
| State | only in `state` | guest memory, and `state` |
| Calls | in parallel | one at a time per instance |
| After a trap | nothing to lose | the instance is dropped; the next call starts afresh |

**Identity is the component, not the stub.** An instance is keyed by the component's SHA-256, its
`export` as written and its `knobs` (as JSON with sorted keys). So:

- Two stubs with the same component, `export` and `knobs` share one instance: a twin split across
  route stubs is one service. Give two copies different `knobs` to keep them apart.
- A `component: "sha256:…"` reference and inline `code` of the same bytes are the same component.
- Unset `export` and `"rift"` are two keys: write it the same way in every stub.

**What keeps it, and what drops it.** Editing stubs keeps an instance for as long as some stub still
references its key: adding or removing other stubs, moving the twin's stub, replacing it with new
predicates or a `_rift.fault`, a `PUT /imposters/:port/stubs` or a reload that keeps it. It is
dropped when:

- no stub references its key any more (a delete, or a replace with other `knobs` or another
  component: changing `knobs` resets a stateful twin, since a guest reads them only once);
- the imposter's runtime state is restarted, or it is deleted or recreated;
- a call traps, runs out of fuel or memory, misses its deadline while the guest runs, or returns with
  its http response body still open.

A guest `err`, or an http component's error response, keeps it. Each drop is logged at `warn` with
the component's short SHA-256 and a reason, and counted in `rift_wasm_instance_drops_total`.

**To open a fault window** in front of a twin, add a stub ahead of it (`POST
/imposters/:port/stubs` at index `0`) and delete that stub afterwards. Deleting the twin's own stub
and adding it back drops the instance.

**A call outlives its client.** It runs on a task of its own, so a client that hangs up, or a delete
that cuts the request, does not stop the guest half-way through changing its memory. The call is
still bounded by `timeoutMs`, and the time a request waits for its instance counts toward it: a
request that times out waiting, or gets the instance with less than 20 ms of its deadline left,
answers a script timeout and drops nothing. A call cut by a restart or a delete finishes on its old
instance, which is not kept, and its `state` operations fail from then on, so it never reads or
writes the next generation's state.

**Each call is its request's.** A kept instance reads the calling request's flow, its random stream
(with `rngKey`, its own key's), its clock and `X-Rift-Now`, and its logs. `wasi:random`'s
insecure seed comes from the instance's key, so a kept instance replays the same way whichever
request creates it.

**Memory.** An instance holds its linear memory, up to `maxMemoryMiB`, for as long as it lives. There
is no cap on how many an imposter keeps: one per distinct key its stubs reference. `GET
/imposters/:port/wasm/instances` lists them, with their memory, and `rift_wasm_instances` counts them.

### Reading a twin's state: `rift:mock/introspect`

With `instance: "imposter"` the guest's memory is its state, which nothing outside it can read. A
component may also export the optional `rift:mock/introspect` interface (in `wit/rift-mock.wit`),
beside `rift:mock/handler` or `wasi:http/incoming-handler`:

```wit
interface introspect {
  snapshot: func() -> result<string, string>;
}
```

`GET /imposters/:port/wasm/snapshot` calls it on a live instance and answers its JSON text byte for
byte, so a harness can check invariants over what the twin holds ("the command was applied at most
once") and not only over what crossed the wire. A component without the export still runs.

**A snapshot does not perturb the run**, so reading an invariant never changes the result being
checked:

- It waits for the instance as a request does, so it never sees a call half-applied.
- `state` is read-only for the call: `set`, `incr-by`, `delete`, `cas`, `ttl`, `key-ttl` and
  `clear` return `err("read-only during snapshot")`. Reads see the imposter's flow store under the
  empty flow id, as a request without `x-flow-id`.
- `wasi:random` draws from a throwaway stream, so a seeded imposter replays the same with or
  without snapshots in between.
- It is not a request: it is not recorded, and it does not count in the instance's `calls`.
- Nothing stops a guest writing its own memory in `snapshot`; one that does breaks a seeded replay.

It runs under the last call's `timeoutMs` and fuel budget. A trap or deadline miss drops the instance,
as on the request path, logged and counted with reason `snapshot-trap`; a guest `err`, or text that
is not JSON, answers `500` and keeps it. With only `instance: "request"` stubs there is no instance
to read: their state is in `state`, which `GET /admin/imposters/:port/flow-state/…` reads.

### Toolchains

Anything that produces a `wasm32-wasip2` component for either world works:

| Language | Tool |
|:---------|:-----|
| Rust | [`wit-bindgen`](https://github.com/bytecodealliance/wit-bindgen) (`rift`), or the [`wasip2`](https://crates.io/crates/wasip2) crate (`http`), and `cargo build --target wasm32-wasip2` |
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

### What a request costs

Each call gets a fresh `Store`: the component is instantiated and `init` runs before `respond`, so
no state survives between requests except what the flow store holds. Measured with
`benches/script_engine_bench.rs` (Apple Silicon, release, a loaded host; see
[Performance](../performance/index.md#scripting-engines) for the full table):

| | wasm | Rhai | JavaScript |
|:--|--:|--:|--:|
| Trivial `respond` answering `200` | 23 µs | 86 µs | 247 µs |
| Plus one flow-store increment | 46 µs | 90 µs | 260 µs |

About 21 µs of the wasm figure is the fresh `Store` and instantiation. Compiling a component is paid
once, at admission (about 22 ms for the 84 KB counter-twin example), and cached after that.

---

## Time

A component's `wasi:clocks/wall-clock` and `monotonic-clock` read the
[imposter clock](../api/index.md#imposter-clock): wall time, an instant a test froze or stepped it
to, or a request's `X-Rift-Now` under `_rift.scriptEngine.clock: "header"`. They are read at every
call, so a step taken while a component runs is seen at its next read (a request's own
`X-Rift-Now` stays fixed for that request). Under a frozen clock the monotonic clock reads the
frozen instant in nanoseconds; otherwise it counts from the start of the process. It never goes back
while the clock's mode and instant do not change, but a change can move it back: unfreezing, or
freezing at an earlier instant. A component that keeps a monotonic reading across calls and
subtracts should allow for that.

A recorded request carries the imposter-clock instant it arrived at (`_clockMs`, with
`_clockSource`) whenever that was not the wall, so a harness that drives the clock can place each
request on its own timeline and check timing across the mock, such as "the retry came within 30 s
of the timeout" ([Recorded requests on the imposter's
timeline](../api/index.md#recorded-requests-on-the-imposters-timeline)).

What the clock does **not** reach: guest timers. `wasi:clocks` `subscribe-duration` and
`subscribe-instant` (a `sleep` in the guest) still wait in real time, count against `timeoutMs`,
and are not woken by a step. A component models a delay by comparing clock reads across calls, not
by sleeping. Nor does it reach `Last-Modified`, the request journal's timestamps, scenario
transition stamps or proxy latency, which stay on real time.

---

## Randomness

A component's `wasi:random` (`random`, `insecure` and `insecure-seed`) draws from the imposter's
random stream. So do `_rift.fault` rolls and the template `randomInt` and `uuid`. With
`_rift.scriptEngine.seed` set, the stream starts from that seed when the imposter is created,
recreated or restarted, so request *n* draws the same values every run:

```json
{ "_rift": { "scriptEngine": { "seed": 42 } } }
```

Without a seed, the stream is seeded from the system's entropy, by the same code: behaviour and cost
are the same either way.

- **What replays.** With a seed and a [frozen clock](../api/index.md#imposter-clock), a sequence of
  requests sent one after another gets the same responses every run. Requests sent concurrently
  draw from the one stream in whatever order they reach it, so which request gets which value
  varies between runs.
- **"Secure" random is deterministic under a seed.** `wasi:random/random` is meant to be a
  cryptographically secure source; behind a seeded imposter it is a seeded generator. This is a
  mock: never seed one that generates real secrets.
- **Every wasm call draws twice** from the stream for its `insecure-seed`, whether or not the
  component uses randomness, so adding a wasm stub to a sequence shifts the draws that follow it.
  The run still replays; it is a different run from the one without that stub.
- **What it does not reach:** the `wait` behaviour's numeric `{min, max}` range, which stays on
  the system's entropy and only changes timing. (A JavaScript `wait` function's `Math.random()`
  does draw from the stream, so it takes its place in a seeded sequence.) JavaScript's `Math.random()` draws from the stream, in a
  `_rift.script` and in Mountebank `inject`/`decorate` functions alike
  ([Time and randomness](scripting.md#time-and-randomness)); Rhai has no random function.

### Concurrent requests: `rngKey`

With one stream per imposter, requests sent concurrently draw in whatever order they reach it, so
a seed alone cannot replay a race (a retry overlapping the original, two callers on one account).
`_rift.scriptEngine.rngKey` gives each request a stream of its own, picked by parts of the request:

```json
{ "_rift": { "scriptEngine": { "seed": 42, "rngKey": ["header:X-Rift-Rng-Key"] } } }
```

- **Parts:** `header:<name>` (case-insensitive, the first value), `query:<name>`, `method`,
  `path`, and `body:<RFC 6901 pointer>` (a string as itself, anything else as JSON text). A test
  that controls its requests sends a header of its own.
- **The stream** comes from the seed, the key, and how many earlier requests had the same key since
  the imposter was created, recreated or restarted. So the third request with key `k` draws the
  same values every run, whenever the other keys' requests arrive.
- **A request without the key** (every part empty: the header missing, the pointer finding
  nothing) uses the imposter's stream. A partly missing key counts the missing part as empty.
- **Every reader** draws from the request's stream: `wasi:random`, `_rift.fault` rolls, the
  template `randomInt` and `uuid`, and JavaScript's `Math.random()`.
- **Admission:** `rngKey` needs `seed`, a non-empty list, and parts Rift can read; otherwise the
  imposter is refused with `400 bad data` naming the part. A config already stored (`--datadir`,
  rift-cluster) still loads, logs the problem, and uses the imposter's stream.
- **Memory:** the per-key counters grow by about 12 bytes for each distinct key, until the next
  restart or recreate. There is no cap.
- `rift wasm check` honours a corpus's `rngKey`.

What stays order-dependent:

- **Two requests in flight with the same key** race for the count. Use a key that differs between
  concurrent requests, such as a correlation id plus an attempt number.
- **State in the flow store** that concurrent requests write.
- **The order responses arrive in**, which is the caller's concern.


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
| `wasi:http/types` | The request and response types of [the `http` export](#the-http-export). It cannot send a request. |
| `wasi:sockets`, `wasi:http/outgoing-handler` | **Not linked.** A component importing them is refused at creation with `wasm component imports <interface>, which Rift does not provide`. |

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
