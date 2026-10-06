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

- Rift must have been started with a config source: `--configfile <file>`,
  `--imposters <uri>[,<uri>...]` (see [imposter sources](../configuration/cli.md#reload-and-etag)),
  or `--datadir <dir>`. Without one, reload is a **no-op** that returns `200` with
  `{"message": "No config source configured; nothing to reload"}`.
- When every `--imposters` source reports it is unchanged (an `http(s):` source answering
  `304 Not Modified`) and there is no `--datadir`, the reload returns `200` without touching
  anything; that body carries the same `created`/`replaced`/`stubPatched`/`deleted` port arrays as
  any other reload report, all empty.
- The new config is **validated in full before** any running imposter is mutated. If it fails to
  parse or has duplicate ports / unsupported protocols, the running imposters are left untouched and
  the call errors. An imposter with no port, or `port: 0`, is auto-assigned and never counts as a
  duplicate. It is never diffed either: each reload deletes and re-creates it on a fresh port, so its
  runtime state resets, and a failure to create it is reported as `auto-assign: <error>` rather
  than under a port. It is created after
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
instead; `POST /admin/reload` re-reads the directory. The directory is keyed by port: every file in
it must declare its `port` (not absent, not `0`) and be named `<port>.json` after it, which is the
name Rift itself writes. A file that breaks either rule refuses the reload with a `500` that names it
and the rule it breaks, including the name it needs when only the name is wrong, and startup skips
it and names it in the log. Rift never renames or edits it.

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
  reload. Remove its file, or delete the imposter, to drop it. The file is replaced atomically, so
  a reload never reads a half-written one. A `<port>.json.tmp` beside it is an interrupted write.
  A reload neither reads nor removes it, and the next start deletes it with a warning.
- A port declared by both the config file and a file in the data directory **refuses the reload**
  with a `500`, and the running imposters are left unchanged. Remove one of the two declarations.
- Every file in the data directory must load. A malformed file, one that declares no `port`, or one
  not named `<port>.json` after its port refuses the reload with a `500` that names it.
  Startup skips such a file and names it in the log instead.
- An imposter from either store that uses a scripting feature (`inject`, `decorate`,
  `shellTransform`, ...) refuses the whole reload with a `400` (`invalid injection`) unless Rift was
  started with `--allowInjection`, again before any running imposter is touched.
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

Reload applies the imposters, the [`routes` block](front-door.md) and the rules of the
[`intercept` block](intercept-proxy.md#declare-it-in-the-config-file).

**Intercept rules reload; the listener does not.** When the intercept listener was started from
this config file's `intercept` block and the block still describes it — the same `host`, `port`,
`auth` and CA source — the block's `rules` replace the rules the file installed, after the
imposters and routes apply. Rules added at runtime with `POST /intercept/rules` stay, after the
file's rules, and the swap is atomic: a request in flight is matched against the old file rules or
the new ones, never neither. The response reports it:

```json
{
  "message": "Reloaded 3 imposter(s)",
  "created": [], "replaced": [4600], "stubPatched": [], "deleted": [],
  "intercept": { "rulesSeeded": 2, "rulesRuntime": 1 }
}
```

After `PUT /intercept/rules` every stored rule counts as a runtime rule, and after
`DELETE /intercept/rules` there are none, so the next reload puts the file's rules back in front.
A reload that applies nothing applies no rules either: when every source reports no change (an
`https:` source answering `304`), it returns before anything runs, and when an imposter fails to
apply (`500`), neither the routes nor the intercept rules change. A rule that uses `inject` without
`--allowInjection` refuses the whole reload before any imposter changes, as a scripted imposter
does.

Rebinding the listener, changing its credential or swapping its CA would break the trust the SUT
already set up, so reload never does any of them. When it cannot apply the block's rules, the
rules stay as they were, there is no `intercept` field, and `warnings` (absent otherwise) says why:

- the block changed a listener field — the warning names which (`host`, `port`, `auth`, `ca`);
  restart to apply it. A CA file pair is compared by its paths, so rotating the files in place also
  needs a restart;
- no listener is running (it was stopped with `DELETE /intercept`); restart to bind it;
- the listener was started from the `--intercept-*` flags, or at runtime with `POST /intercept` or
  FFI — the file's block does not own its rules;
- the file no longer declares a block; the listener it started keeps running with its rules.

```json
{
  "message": "Reloaded 3 imposter(s)",
  "warnings": [
    "the config file changed the intercept listener's port; the listener is started at boot only, so its rules were NOT re-applied — restart to apply the block"
  ],
  "created": [], "replaced": [], "stubPatched": [], "deleted": []
}
```

A [`routes` block](front-door.md) **is** re-applied. After the imposters apply successfully, the
front door switches to the reloaded table in one step: requests after the reload use the new
routes, and no listener is rebound. Removing the block reloads to an empty table, as a restart
would. An invalid table refuses the whole reload, and a reload that fails leaves the old table
serving. A config file with a `routes` block on a server started without `--front-door` has
nothing to apply it to. The block is ignored, and the startup log and every reload response's
`warnings` say so.

If some ports apply and others fail, the call returns `500` and reports both sides — the ports that
did apply and the ones that failed:

```json
{
  "errors": [{ "code": "500", "type": "internal error", "message": "Reload partially failed: ..." }],
  "failed": ["4545: ...", "auto-assign: ..."],
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
A server started with `--no-parse` (or an embedded host with the `noParse` serve option) re-reads
the file verbatim, so a literal `<%` reloads as written.

When the failure is a source that could not be fetched, the message carries the whole cause chain,
so it names the specific reason rather than a generic transport error — for example
`Reload failed (imposters unchanged): fetching imposter source https://host/imposters.json: error
following redirect for url (…): too many redirects (limit 10): https://host/loop`. The redirect cap
is Rift's own (10 hops), and the message names the URL it gave up on.

> Embedders can also observe the diff programmatically: an incremental apply emits imposter change
> events (`Created` / `Replaced` / `StubsChanged` / `Deleted`) to any registered listener.
