---
layout: default
title: Hot Reload
parent: Features
nav_order: 26
---

# Hot Reload

`POST /admin/reload` re-reads the startup config source and applies it **incrementally** — Rift
diffs the running imposters against the new config and touches only what actually changed. Editing
an imposter in a file and reloading no longer tears every imposter down.

---

## Requirements & behavior

- Rift must have been started with a config source: `--configfile <file>` or `--datadir <dir>`.
  Without one, reload is a **no-op** that returns `200`.
- The new config is **validated in full before** any running imposter is mutated. If it fails to
  parse or has duplicate ports / unsupported protocols, the running imposters are left untouched and
  the call errors.
- The reload is **incremental** (issue #319): each port is diffed and only the delta is applied.
  Unchanged imposters — and unchanged stubs within a changed imposter — **keep their runtime
  state**: recorded requests, scenario state, and response cyclers (`repeat`) all survive the
  reload.

```bash
rift --configfile ./imposters.json      # start with a config source

# ...edit imposters.json...

curl -X POST http://localhost:2525/admin/reload   # 200; delta applied, state preserved
```

To reload from a directory of one-imposter-per-file configs, start with `--datadir ./mb-data`
instead; `POST /admin/reload` re-reads the directory.

---

## What the diff does

Rift computes the change set per port and classifies each imposter:

- **created** — a port present in the new config but not running.
- **deleted** — a running port absent from the new config.
- **replaced** — an imposter whose imposter-level fields changed, or whose stub set changed so
  substantially (more than ~50% of stubs) that an in-place patch is not worthwhile. A replaced
  imposter starts with fresh runtime state.
- **stubPatched** — an imposter whose stubs changed only modestly; the differing stubs are patched
  in place and every unchanged stub keeps its cursor/scenario state.

Stubs are matched across a reload by a **stable key**: a stub's explicit `id` if it has one,
otherwise a content hash. Reordering stubs or editing a neighbour therefore preserves the state of
the stubs you didn't touch.

## Reload response

A successful reload returns `200` with the change set:

```json
{
  "message": "Reloaded 3 imposter(s)",
  "created": [4547],
  "replaced": [4545],
  "stubPatched": [4546],
  "deleted": [4544]
}
```

If some ports apply and others fail, the call returns `500` and reports both sides — the ports that
did apply and the ones that failed:

```json
{
  "errors": [{ "code": "500", "message": "Reload partially failed: ..." }],
  "failed": ["4545: ..."],
  "created": [],
  "replaced": [],
  "stubPatched": [],
  "deleted": []
}
```

A validation failure that is caught **before** any mutation returns `500` with an `errors` array and
leaves every running imposter in place.

> Embedders can also observe the diff programmatically: an incremental apply emits imposter change
> events (`Created` / `Replaced` / `StubsChanged` / `Deleted`) to any registered listener.
