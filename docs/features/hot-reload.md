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
  the call errors. An imposter with no port, or `port: 0`, is auto-assigned and never counts as a
  duplicate. It is never diffed either: each reload deletes and re-creates it on a fresh port, so its
  runtime state resets, and a failure to create it is reported under port `0`. It is created after
  every imposter with an explicit port, so the fresh port is never one an explicit imposter in the
  config is serving.
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

### A config file and a data directory together

`rift --configfile imposters.json --datadir ./mb-data` seeds imposters from the file and persists
the ones created through the admin API to the directory. `--imposters` behaves the same as
`--configfile`. `POST /admin/reload` re-reads **both** and applies them as one set, so each store
keeps its own imposters:

- At startup both stores are created in one pass, every imposter with a port before any imposter
  without one. An imposter without a port is given the lowest free port from 49152, so this is what
  stops it taking a port a file in the data directory declares.
- An imposter from the config file is **never written to the data directory**, at startup or on
  reload. Neither are changes made to it at runtime, through the stub endpoints or a
  `PUT /imposters` that repeats it. The file is where it is re-read from. An imposter without a port
  in a `PUT /imposters` cannot be matched to a running one, so it is created and persisted like any
  other admin-API imposter.
- An imposter created with `POST /imposters` is written to `<datadir>/<port>.json` and survives a
  reload. Remove its file, or delete the imposter, to drop it.
- A port declared by both the config file and a file in the data directory **refuses the reload**
  with a `500`, and the running imposters are left unchanged. Remove one of the two declarations.
- Every file in the data directory must load. A malformed file refuses the reload with a `500` that
  names it, and a file that uses a scripting feature refuses it unless `--allowInjection` is set.
  Startup skips such a file and names it in the log instead.
- A data directory has no change marker, so a reload with one always runs the diff, even when every
  config source reports it is unchanged.

Before this behaviour (issue #1122), a server run with both flags wrote a `<port>.json` copy of every
config-file imposter into the data directory, and the first reload deleted every imposter that only
the directory declared, together with its file. If you ran both flags on an earlier release, delete
the copies: a copy of an imposter with an explicit port now refuses every reload, and a copy of one
without a port is served as a second imposter.

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

Reload applies **imposters only**. If the config file also declares an
[`intercept` block](intercept-proxy.md#declare-it-in-the-config-file), it is *not* re-applied —
re-seeding would duplicate or clobber rules added at runtime, and rebinding the listener is a
lifecycle change reload does not own. So that an edit to the block never *looks* applied, the
response says so explicitly (the field is absent otherwise):

```json
{
  "message": "Reloaded 3 imposter(s)",
  "warnings": [
    "the config file's `intercept` block is applied at startup only and was NOT re-applied; ..."
  ],
  "created": [], "replaced": [], "stubPatched": [], "deleted": []
}
```

Change intercept rules at runtime with `POST`/`DELETE /intercept/rules`, or restart to re-read the
block.

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
leaves every running imposter in place. An EJS tag the preprocessor does not evaluate is one such
failure; the message names the tag and its line, for example
``Reload failed (imposters unchanged): unsupported EJS tag `<% if (x) { %>` at imposters.json:3, …``.

When the failure is a source that could not be fetched, the message carries the whole cause chain,
so it names the specific reason rather than a generic transport error — for example
`Reload failed (imposters unchanged): fetching imposter source https://host/imposters.json: error
following redirect for url (…): too many redirects`.

> Embedders can also observe the diff programmatically: an incremental apply emits imposter change
> events (`Created` / `Replaced` / `StubsChanged` / `Deleted`) to any registered listener.
