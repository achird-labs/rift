---
layout: default
title: FFI (C-ABI)
parent: Embedding & SPI
nav_order: 3
---

# FFI (C-ABI)

`rift-ffi` exposes a stable **C-ABI (v2)** so any language with C interop — the JVM, Node, Go,
Python, … — can embed Rift in-process without shelling out to the binary. It builds as a `cdylib`
(and an `rlib` for in-crate tests): `crate-type = ["cdylib", "rlib"]`.

A cbindgen-generated header ships in the repo at **`crates/rift-ffi/include/rift_ffi.h`**
(regenerate/verify with `scripts/verify-ffi-cdylib.sh`). Do not hand-edit it.

---

## Handle lifecycle

The ABI is built around an opaque `RiftHandle` (a Tokio runtime + an `Arc<ImposterManager>`, plus an
optional in-process admin/metrics plane).

```c
RiftHandle* h = rift_start();     // create
// ... drive it ...
rift_stop(h);                     // stop and free
```

| Function | Signature | Purpose |
|:---------|:----------|:--------|
| `rift_start` | `RiftHandle* rift_start(void)` | Create a handle (owns a runtime + manager). |
| `rift_stop` | `void rift_stop(RiftHandle* h)` | Stop the handle's servers and free it. |

---

## Imposter operations

| Function | Signature | Returns |
|:---------|:----------|:--------|
| `rift_create_imposter` | `uint16_t rift_create_imposter(RiftHandle* h, const char* json)` | The imposter port, or **`0` on any error** (`0` is never a live port). |
| `rift_replace_stubs` | `int rift_replace_stubs(RiftHandle* h, uint16_t port, const char* json)` | `0` on success, `-1` on error. |
| `rift_delete_imposter` | `int rift_delete_imposter(RiftHandle* h, uint16_t port)` | `0` on success, `-1` on error. |
| `rift_delete_all` | `int rift_delete_all(RiftHandle* h)` | `0` on success, `-1` on error. |
| `rift_recorded` | `char* rift_recorded(RiftHandle* h, uint16_t port)` | Recorded requests as a JSON string (**caller frees** with `rift_free`), or `NULL` on error. |
| `rift_verify` | `char* rift_verify(RiftHandle* h, uint16_t port, const char* body_json)` | Server-side verification: given `{"predicates":[…],"flowId"?,"includeRequests"?,"includeClosest"?}` (the [`POST /verify`](../api/index.md#post-impostersportverify) body), returns `{"matched","total","requests"?,"closest"?}` as JSON (**caller frees**), or `NULL` on error. Unlike the HTTP endpoint, `inject` predicates are **not** gated — the in-process embedder is trusted. |
| `rift_stub_warnings` | `char* rift_stub_warnings(RiftHandle* h, uint16_t port)` | [Stub-analysis warnings](../features/stub-analysis.md) (duplicate/shadowed/catch-all) as a JSON array (**caller frees**), or `NULL` on error. |
| `rift_apply_config` | `char* rift_apply_config(RiftHandle* h, const char* json)` | Reconcile the full imposter set (like `POST /admin/reload`); returns the apply report JSON (caller frees). |

## Admin long tail over FFI

The admin "long tail" — scenario/flow-state, the correlated per-space stub plane, imposter list/get,
stub surgery, and scenario management — has direct C-ABI entry points, so an embedder can drive it
with **zero loopback HTTP** (no `rift_serve_admin`). Each mirrors the corresponding admin-HTTP
handler exactly (same `ImposterManager`/`Imposter` calls, same JSON):

| Function | Signature | Returns |
|---|---|---|
| `rift_flow_state_get` | `char* rift_flow_state_get(RiftHandle* h, uint16_t port, const char* flow_id, const char* key)` | JSON envelope `{"found","flowId","key","value"}` (**caller frees**); `found:false` (with `value:null`) is an absent key, `NULL` is returned **only** on error. |
| `rift_flow_state_put` | `int rift_flow_state_put(RiftHandle* h, uint16_t port, const char* flow_id, const char* key, const char* value_json)` | `0` on success, `-1` on error. `value_json` is the bare JSON value. |
| `rift_flow_state_delete` | `int rift_flow_state_delete(RiftHandle* h, uint16_t port, const char* flow_id, const char* key)` | `0` on success, `-1` on error. |
| `rift_space_add_stub` | `int rift_space_add_stub(RiftHandle* h, uint16_t port, const char* flow_id, const char* stub_json)` | `0` on success, `-1` on error. The stub's `space` is set from `flow_id`. |
| `rift_space_list_stubs` | `char* rift_space_list_stubs(RiftHandle* h, uint16_t port, const char* flow_id)` | JSON `{"space","stubs":[…]}` (**caller frees**), or `NULL` on error. |
| `rift_space_delete` | `int rift_space_delete(RiftHandle* h, uint16_t port, const char* flow_id)` | `0` on success, `-1` on error. One-call per-space teardown (scoped stubs + recorded + scenario state). |
| `rift_space_recorded` | `char* rift_space_recorded(RiftHandle* h, uint16_t port, const char* flow_id)` | The requests recorded for that space (header-filtered `received`) as a JSON array (**caller frees**), or `NULL` on error. |

`rift_flow_state_get` never conflates "absent" with "failed": an absent key returns the envelope
with `found:false` (a normal outcome, `rift_last_error` untouched), and `NULL` is reserved strictly
for a genuine error — so a consumer treats `found:false` as "unset" and `NULL` as a read failure,
with no need to parse `rift_last_error`.

The rest of the admin long tail — imposter list/get, stub surgery (add/get/update/delete, by index
or id), clearing recorded/proxy recordings, enable/disable, and scenario list/set-state/reset — is
likewise direct C-ABI, each calling the same `ImposterManager`/`Imposter` method the corresponding
admin-HTTP handler calls:

| Function | Signature | Returns |
|---|---|---|
| `rift_list_imposters` | `char* rift_list_imposters(RiftHandle* h, const char* options_json)` | JSON `{"imposters":[...]}` (**caller frees**), or `NULL` on error. `options_json` (`NULL` = defaults): `{"replayable":false,"removeProxies":false}`. `replayable` returns full `ImposterConfig`s (the same `removeProxies` projection the admin `?replayable=true` route serves); otherwise a summary shape `{"protocol","port","name"?,"numberOfRequests","enabled"}` per imposter (imposters with no assigned port are skipped). |
| `rift_get_imposter` | `char* rift_get_imposter(RiftHandle* h, uint16_t port, const char* options_json)` | Same `options_json` shape as `rift_list_imposters`. Replayable returns the single `ImposterConfig`; otherwise a detail object `{"protocol","port","name"?,"numberOfRequests","enabled","recordRequests","stubs","requests"}` (**caller frees**), or `NULL` on error. |
| `rift_add_stub` | `int rift_add_stub(RiftHandle* h, uint16_t port, const char* stub_json, int32_t index)` | `0` on success, `-1` on error. `index < 0` appends; otherwise inserts at that position. No stub id is auto-generated; no injection gating (the direct C-ABI is the trusted embedder, like `rift_replace_stubs`). |
| `rift_get_stub` | `char* rift_get_stub(RiftHandle* h, uint16_t port, const char* ref_json)` | The bare `Stub` JSON (**caller frees**), or `NULL` on error (out-of-range index, unknown id, or malformed ref). `ref_json` is `{"index":N}` or `{"id":"..."}`. |
| `rift_update_stub` | `int rift_update_stub(RiftHandle* h, uint16_t port, const char* ref_json, const char* stub_json)` | `0` on success, `-1` on error. Replaces the stub addressed by `ref_json` with `stub_json`. |
| `rift_delete_stub` | `int rift_delete_stub(RiftHandle* h, uint16_t port, const char* ref_json)` | `0` on success, `-1` on error. |
| `rift_clear_recorded` | `int rift_clear_recorded(RiftHandle* h, uint16_t port)` | `0` on success, `-1` on error. Clears all recorded requests for the imposter. |
| `rift_clear_proxy_recordings` | `int rift_clear_proxy_recordings(RiftHandle* h, uint16_t port)` | `0` on success, `-1` on error. Clears saved proxy responses. |
| `rift_set_imposter_enabled` | `int rift_set_imposter_enabled(RiftHandle* h, uint16_t port, int32_t enabled)` | `0` on success, `-1` on error. `enabled != 0` enables; `0` disables. |
| `rift_scenarios` | `char* rift_scenarios(RiftHandle* h, uint16_t port, const char* flow_id)` | JSON `{"flowId","scenarios":[{"name","state"}]}` (**caller frees**), or `NULL` on error. `flow_id` may be `NULL` for the imposter's default flow. |
| `rift_set_scenario_state` | `int rift_set_scenario_state(RiftHandle* h, uint16_t port, const char* name, const char* state_json)` | `0` on success, `-1` on error (including a missing `state` field). `state_json`: `{"state":"...","flowId":"..."?}` (`flowId` optional → default flow). |
| `rift_reset_scenarios` | `int rift_reset_scenarios(RiftHandle* h, uint16_t port, const char* flow_id)` | `0` on success, `-1` on error. Resets every scenario for `flow_id` (`NULL` → default flow) back to its initial state. |

Errors set `rift_last_error` like the data-plane functions. Together with the data plane
(`rift_create_imposter`/`rift_replace_stubs`/`rift_recorded`/`rift_delete_imposter`), these cover the
whole SPI over C-ABI — an embedded consumer needs no admin HTTP server and no loopback client.

## In-process admin plane (optional)

`rift_serve_admin` starts the **real** admin API (and, if `metricsPort` is set, the metrics server)
on the handle's runtime, serving the handle's manager — so external tooling can talk to an embedded
Rift over HTTP. It is **optional**: the direct C-ABI above already covers the admin long tail; use
`rift_serve_admin` only when you want an actual HTTP admin surface.

```c
char* result = rift_serve_admin(h, "{\"port\":0}");
// result: {"adminPort":49321,"adminUrl":"http://127.0.0.1:49321","metricsPort":null}
rift_free(result);
```

- **Options JSON** (pass `NULL` or `{}` for all defaults; every field optional):
  `{"host":"127.0.0.1","port":0,"apiKey":null,"metricsPort":null,"configFile":null,"config":null,"allowInjection":false}`.
  `port: 0` binds an ephemeral port; `configFile` is loaded as the reload source (like `--configfile`);
  `config` is an inline `{"imposters":[...]}`. `configFile` and `config` do not compose — pass one.
- **`allowInjection`** (default `false`): whether the admin plane accepts script/`inject` imposters
  submitted **through it** (`POST /imposters` etc.), mirroring the `--allowInjection` CLI flag.
  Note the deliberate asymmetry: **direct FFI calls** (`rift_create_imposter`, `rift_replace_stubs`,
  …) are **ungated** — the host process is already trusted, so script imposters always work over the
  C-ABI. `allowInjection` only governs the in-process HTTP admin surface; leave it `false` unless you
  expose that surface to less-trusted callers and want script imposters permitted there too.
- **Returns** (caller frees): `{"adminPort":...,"adminUrl":"...","metricsPort":...}`, or `NULL` on
  error (bad JSON, bind failure, or already serving — one admin plane per handle).

## Intercept proxy over FFI

Start the [intercept/TLS-MITM proxy]({{ site.baseurl }}/features/intercept-proxy/) on the handle and
drive its whole control plane over C-ABI — no in-process admin HTTP needed. One intercept listener
per handle; `rift_stop_intercept` stops it, and `rift_stop` shuts it down with the handle.

| Function | Signature | Returns |
|---|---|---|
| `rift_start_intercept` | `char* rift_start_intercept(RiftHandle* h, const char* options_json)` | JSON `{"interceptPort","interceptUrl"}` (**caller frees**), or `NULL` on error (bad JSON, bind failure, half-configured CA pair, CA load failure, already started). `options_json`: `{"host":"127.0.0.1","port":0,"caCertPath":null,"caKeyPath":null}` (port 0 = OS-assigned); `NULL`/`{}` for defaults. |
| `rift_stop_intercept` | `int rift_stop_intercept(RiftHandle* h)` | `0` on success (**including** the idempotent nothing-running case), `-1` only on a null handle / caught panic. Stops the listener, releases its port, and drops its rules + CA — RFC-003 parity with `DELETE /intercept`. A later `rift_start_intercept` without CA paths mints a fresh CA. |
| `rift_intercept_add_rules` | `int rift_intercept_add_rules(RiftHandle* h, const char* rules_json)` | `0`/`-1`. One rule (object) or many (array), same shape as `/intercept/rules`. |
| `rift_intercept_list_rules` | `char* rift_intercept_list_rules(RiftHandle* h)` | The current rules as a JSON array (**caller frees**), or `NULL` on error. |
| `rift_intercept_clear_rules` | `int rift_intercept_clear_rules(RiftHandle* h)` | `0`/`-1`. |
| `rift_intercept_ca_pem` | `char* rift_intercept_ca_pem(RiftHandle* h)` | The CA cert PEM (**caller frees**), or `NULL` on error. |
| `rift_intercept_export_truststore` | `int rift_intercept_export_truststore(RiftHandle* h, const char* format, const char* password, const char* out_path)` | Writes a truststore to `out_path` (`format` = `"pkcs12"`/`"jks"`, `password` may be `NULL` → `"changeit"`). `0`/`-1`. A truststore is binary, so it is written to a file — the form a JVM `trustStore` consumes directly. |

An embedder calls `rift_start_intercept`, reads `interceptPort`, adds rules and fetches the CA /
truststore (all over FFI), then points a CA-trusting SUT's HTTPS proxy at `interceptPort` — with no
loopback HTTP. `Forward { port }` rules reach any imposter on that localhost port, including
FFI-created ones. Errors set `rift_last_error`. A handle that never calls `rift_start_intercept` is
unaffected.

The intercept listener and the handle's embedded admin plane share one slot (#493): if you also call
`rift_serve_admin`, its `/intercept*` routes operate on the **same** listener the C-ABI functions
drive. `rift_start_intercept` then `GET /intercept` reports it; `POST /intercept` feeds
`rift_intercept_add_rules`; and a double-start across the two surfaces conflicts consistently
(`409` / `-1`). (Previously `rift_serve_admin` served no `/intercept` routes at all.)

By default (no `caCertPath`/`caKeyPath`) `rift_start_intercept` generates a fresh ephemeral intercept
CA, unchanged from earlier releases. As of v0.11.3 (#429), passing both `caCertPath` and `caKeyPath`
(PEM file paths) loads that committed CA instead — letting independent embedded instances share one
trust anchor rather than each minting its own. Passing only one of the pair is a hard error (both or
neither).

`interceptUrl`/`interceptPort` in the response are derived from the listener's **actual bound
address** (v0.11.2, #425/#426) — not hardcoded to `127.0.0.1`. A loopback `host` (the default)
reports loopback as before; a `0.0.0.0` (or other non-loopback) bind surfaces that address verbatim,
so dial a concrete interface rather than assuming `127.0.0.1`.

## Build identity

`rift_build_info` is a **static** JSON string (never freed) — probe it to detect a v2 library and read
which engines are compiled in:

```c
const char* info = rift_build_info();
// {"version":"0.11.3","commit":"<sha>|null","builtAt":"<iso8601>|null","features":["redis-backend","javascript"]}
// Do NOT call rift_free on this pointer.
```

`commit`/`builtAt` are `null` unless stamped at build time (via `build.rs`).

---

## Error handling & ownership conventions

- **Error signaling.** Every *operation* returns a sentinel on failure — `0` (`rift_create_imposter`),
  `-1` (the `int`-returning ops), or `NULL` (the string-returning ops).
- **`rift_last_error`.** `char* rift_last_error(void)` returns the last error message for the current
  thread (**caller frees**), or `NULL` if none. Every operation entry **clears** the thread-local
  error first and sets it on failure, so read it immediately after a sentinel return.
- **Panic safety.** A Rust panic inside any operation is caught at the boundary and turned into that
  function's sentinel plus a `rift_last_error` message (never unwinds across the C ABI, never crashes
  or wedges the host runtime) — so an engine bug degrades to a normal error return you handle exactly
  like any other failure.
- **String ownership.** Every `char*` the ABI returns must be released with
  `void rift_free(char* p)` — **except** `rift_build_info`'s pointer, which is static and must not be
  freed. `rift_free(NULL)` is a safe no-op.
- **Handle ownership.** Free a handle exactly once with `rift_stop`.

---

## Cargo features

`rift-ffi` forwards engine features rather than hard-coding them, so a per-platform build can drop
engines it doesn't need: `default = ["redis-backend", "javascript"]`. The `mimalloc` allocator
feature is **deliberately never forwarded** — a `cdylib` must not impose a global allocator on its
host process.

```sh
# Full-featured cdylib (default)
cargo build -p rift-ffi --release

# Minimal cdylib — no scripting engines, no Redis
cargo build -p rift-ffi --release --no-default-features
```

## Prebuilt cdylibs & the release manifest

Every release ships the `librift_ffi` cdylib as a standalone, classifier-named asset per platform
(`librift_ffi-<classifier>.{so,dylib,dll}`, e.g. `librift_ffi-linux-x86_64.so`,
`librift_ffi-darwin-aarch64.dylib`, `librift_ffi-windows-x86_64.dll`) alongside a matching
`.sha256`. The x86_64 musl target ships a cdylib too (`librift_ffi-linux-x86_64-musl.so`) — built
with `crt-static` disabled so the `.so` links dynamically against Alpine's musl libc (the
platform's static binaries stay statically linked), for embedded SDK tests on Alpine CI images.

Each cdylib is **self-contained**: it links only stock-host system libraries (the C runtime and,
on macOS, system frameworks) — no third-party native dependency to install. A release gate asserts
this per platform (`scripts/check-ffi-selfcontained.sh`, issue #469), so a consumer can `dlopen`
the library on a bare host without extra packages.

To let SDK consumers (rift-java's natives packaging, rift-go's fetcher/loader, spawn-transport
binary downloads) resolve these assets without hardcoding release-URL patterns, each release also
publishes **`ffi-manifest.json`** — a platform → asset map generated by
`scripts/gen-ffi-manifest.sh` from the per-platform `.sha256` assets:

```json
{
  "version": "v0.12.0",
  "abi": "v2",
  "artifacts": [
    {
      "platform": "linux-x86_64",
      "file": "librift_ffi-linux-x86_64.so",
      "sha256": "…",
      "url": "https://github.com/EtaCassiopeia/rift/releases/download/v0.12.0/librift_ffi-linux-x86_64.so"
    }
  ]
}
```

Fetch it at `https://github.com/EtaCassiopeia/rift/releases/download/<version>/ffi-manifest.json`,
pick the `artifacts[]` entry matching the host's `platform`, download `url`, and verify against
`sha256`. `abi` is the C-ABI major version (`v2`) the cdylibs export.
