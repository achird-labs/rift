# Rift SDK conformance corpus

This directory is published per Rift release as **`sdk-conformance-<version>.tar.gz`** (a GitHub
release asset). Every official Rift SDK (rift-java, rift-go, rift-scala, rift-node) replays this
corpus in its CI. It is the single source of truth for **DSL ↔ engine parity**: a fixture the SDK's
typed DSL cannot express is a red SDK build (RFC-003 §9.2, risk R1).

The corpus is **engine-canonical**: it lives here, in the engine repo, alongside the `_verify`
schema (issue #251) and its reference replayer `rift-verify`, and is version-locked to the engine
release it ships with. An engine CI gate (`crates/rift-http-proxy/tests/corpus_replay.rs`) replays
the whole corpus on every commit, so a published artifact is verified, never merely hoped.

## Layout

```
sdk-conformance-<version>/
├── README.md            # this file — the normative replay contract
├── manifest.json        # machine-readable index (schemaVersion, engineVersion, fixtures[])
└── corpus/
    ├── imposters/NN-name.json   # a standard Mountebank/Rift imposter config, optionally with
    │                            #   `_verify` (expected request/response transcripts) and `_behaviors`
    ├── data/…                   # data files referenced by fixtures as `data/<file>` (cwd = corpus/)
    └── fixtures/injection/*.{js,cjs}   # JS decorate/inject modules referenced by injection fixtures
```

Paths inside a fixture (e.g. `"path": "data/products.csv"`) are **relative to `corpus/`** — the
replayer's working directory must be `corpus/`. Do not rewrite them; embedded lanes that cannot set
a working directory should absolutize them at load time against the extracted `corpus/` root.

## `manifest.json`

```json
{
  "schemaVersion": 1,
  "engineVersion": "X.Y.Z",
  "fixtures": [
    { "file": "corpus/imposters/05-scripting-engines.json", "port": 4505,
      "name": "05 · Scripting engines", "requires": ["injection"], "hasVerify": false }
  ]
}
```

- **`engineVersion`** — the Rift release this corpus ships with. Corpus version **==** engine
  release version; there is no independent corpus versioning.
- **`requires`** — capability gates from the closed set
  `["injection", "proxy", "redis", "https", "shell"]`. An SDK CI may skip a fixture **only** when
  its lane lacks a capability the fixture declares — never ad hoc. (`injection` fixtures need the
  engine started with `--allowInjection`; `proxy` fixtures need an upstream — `rift-verify` stands
  up its own; `https`/`redis`/`shell` gate on TLS / a Redis backend / a host shell.)
- **`hasVerify`** — the fixture carries a `_verify` sequence (see below).

## The replay contract

For each fixture in `manifest.json`, an SDK conformance suite MUST:

1. **DSL-expressibility gate.** Parse the fixture JSON and reconstruct the imposter through the
   SDK's typed DSL. The DSL's serialized output must **deep-equal** the fixture (JSON object key
   order is irrelevant). Permitted normalizations are exactly those `rift-lint` treats as
   equivalent — chiefly omitted-vs-explicit defaults (e.g. an absent `protocol` normalizing to
   `"http"`). A fixture that **cannot** be expressed through the typed DSL is a **red build**: it
   means the DSL has drifted from the engine grammar.

2. **Serve & replay.** Create the imposter (from the DSL-built form) and, when the fixture has a
   `_verify` sequence, drive each `sequence[].request` in order and assert each `sequence[].expect`.
   The `_verify` schema is engine-native (see `docs/configuration/cli.md` and
   `docs/features/stub-analysis.md`); `expect` keys include `status` and `bodyContains`. When in
   doubt about semantics, assert exactly what the reference replayer asserts:

   ```
   rift-verify --admin-url <admin> --skip-dynamic --verify-dynamic
   ```

   (`--skip-dynamic` skips inject/proxy/script stubs in the *static* pass; `--verify-dynamic`
   replays the `_verify` sequences and asserts deterministic `_rift.fault` outcomes.) It exits
   non-zero iff any assertion fails.

3. **Both transports.** Run the same fixtures with the same assertions over every transport the SDK
   supports — **embedded** (C-ABI / FFI) and **remote** (admin API). A fixture must behave
   identically on each.

4. **Capability skips only per the manifest.** Skip a fixture solely because its `requires` names a
   capability the lane lacks (e.g. an embedded lane on a platform without the cdylib, per the
   release FFI manifest; an `injection` fixture when the lane runs without `--allowInjection`).
   Never skip a fixture for any other reason.

5. **Versioning.** An SDK pinning engine `X.Y.Z` downloads `sdk-conformance-X.Y.Z.tar.gz` from that
   engine release and replays it. The corpus and the engine move together.

## Extending the corpus

Fixtures are **numbered and append-only** (`NN-name.json`, never renumbered) so a fixture's identity
is stable across versions. Add a new fixture with the next free number, register it in
`manifest.json` (with its `port`, `requires`, and `hasVerify`), and the engine gate
(`corpus_replay.rs`) will enforce that it serves and its `_verify` transcripts hold before it can
ship. Seed material for new fixtures lives in the engine's `tests/compatibility` and `examples/`.
