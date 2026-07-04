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
| `rift_apply_config` | `char* rift_apply_config(RiftHandle* h, const char* json)` | Reconcile the full imposter set (like `POST /admin/reload`); returns the apply report JSON (caller frees). |

## In-process admin plane

`rift_serve_admin` starts the **real** admin API (and, if `metricsPort` is set, the metrics server)
on the handle's runtime, serving the handle's manager — so external tooling can talk to an embedded
Rift over HTTP.

```c
char* result = rift_serve_admin(h, "{\"port\":0}");
// result: {"adminPort":49321,"adminUrl":"http://127.0.0.1:49321","metricsPort":null}
rift_free(result);
```

- **Options JSON** (pass `NULL` or `{}` for all defaults; every field optional):
  `{"host":"127.0.0.1","port":0,"apiKey":null,"metricsPort":null,"configFile":null,"config":null}`.
  `port: 0` binds an ephemeral port; `configFile` is loaded as the reload source (like `--configfile`);
  `config` is an inline `{"imposters":[...]}`. `configFile` and `config` do not compose — pass one.
- **Returns** (caller frees): `{"adminPort":...,"adminUrl":"...","metricsPort":...}`, or `NULL` on
  error (bad JSON, bind failure, or already serving — one admin plane per handle).

## Build identity

`rift_build_info` is a **static** JSON string (never freed) — probe it to detect a v2 library and read
which engines are compiled in:

```c
const char* info = rift_build_info();
// {"version":"0.8.0","commit":"<sha>|null","builtAt":"<iso8601>|null","features":["redis-backend","lua","javascript"]}
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
- **String ownership.** Every `char*` the ABI returns must be released with
  `void rift_free(char* p)` — **except** `rift_build_info`'s pointer, which is static and must not be
  freed. `rift_free(NULL)` is a safe no-op.
- **Handle ownership.** Free a handle exactly once with `rift_stop`.

---

## Cargo features

`rift-ffi` forwards engine features rather than hard-coding them, so a per-platform build can drop
engines it doesn't need: `default = ["redis-backend", "lua", "javascript"]`. The `mimalloc` allocator
feature is **deliberately never forwarded** — a `cdylib` must not impose a global allocator on its
host process.

```sh
# Full-featured cdylib (default)
cargo build -p rift-ffi --release

# Minimal cdylib — no scripting engines, no Redis
cargo build -p rift-ffi --release --no-default-features
```
