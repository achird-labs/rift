---
layout: default
title: SDK conformance corpus
parent: Embedding & SPI
nav_order: 4
---

# SDK conformance corpus

Every Rift release publishes **`sdk-conformance-<version>.tar.gz`** as a GitHub release asset. It is
the shared corpus that every official Rift SDK (rift-java, rift-go, rift-scala, rift-node) replays in
its CI to prove its typed DSL stays in lockstep with the engine grammar — a fixture the DSL cannot
express is a red SDK build.

The corpus is **engine-canonical**: it is vendored in this repo under `sdk-conformance/`, alongside
the `_verify` schema and its reference replayer `rift-verify`, and is version-locked to the release
it ships with. An engine CI gate (`crates/rift-http-proxy/tests/corpus_replay.rs`) replays the whole
corpus on every commit, so a published artifact is always verified rather than merely hoped.

## Artifact layout

```
sdk-conformance-<version>/
├── README.md            # the normative replay contract (packaged in the tarball)
├── manifest.json        # { schemaVersion, engineVersion, fixtures[] }
└── corpus/
    ├── imposters/NN-name.json   # imposter config, optionally with `_verify` transcripts
    ├── data/…                   # data files referenced as `data/<file>` (cwd = corpus/)
    └── fixtures/injection/*.{js,cjs}
```

`manifest.json` indexes each fixture with its `port`, a `name`, a `requires` capability list (closed
set: `injection`, `proxy`, `redis`, `https`, `shell`), and `hasVerify`. `engineVersion` equals the
Rift release — the corpus and engine move together, so an SDK pinning engine `X.Y.Z` downloads
`sdk-conformance-X.Y.Z.tar.gz` from that release.

## The replay contract (for SDK authors)

For each fixture, an SDK conformance suite must:

1. **Express it through the typed DSL** and assert the DSL's serialized output deep-equals the
   fixture (a fixture the DSL cannot express is a red build).
2. **Serve and replay** — create the imposter and, when it has a `_verify` sequence, drive each
   request and assert each expectation. The reference semantics are
   `rift-verify --skip-dynamic --verify-dynamic`; when in doubt, assert what it asserts.
3. **Run both transports** it supports — embedded (C-ABI / FFI) and remote (admin API) — with
   identical assertions.
4. **Skip a fixture only** when its `requires` names a capability the lane lacks (e.g. an
   `injection` fixture without `--allowInjection`), never ad hoc.

The full contract ships in the tarball's `README.md`. See also the
[`_verify` annotations]({{ site.baseurl }}/configuration/cli/) documented for `rift-verify`.

## Replaying against an externally-supplied server

`crates/rift-http-proxy/tests/corpus_replay.rs`, and the other process-spawning suites under
`crates/rift-http-proxy/tests/` (`mountebank_compatibility.rs`, `rift_extensions.rs`,
`issue_360_script_cli.rs`), default to spawning this repo's own debug build of `rift-http-proxy`.
Set **`RIFT_SERVER_BIN`** to an existing, executable path to run them against any other
Mountebank-compatible build instead — a differently-packaged rift (e.g. a musl static image) or a
downstream binary that composes rift's `ServerBuilder`. A set-but-missing/non-executable path
panics rather than silently falling back. Only the server is overridable this way; the reference
`_verify` replayer `rift-verify` (used by `corpus_replay.rs`) always stays the debug build built
alongside the tests.

```sh
cargo build -p rift-http-proxy
RIFT_SERVER_BIN=$PWD/target/debug/rift-http-proxy \
  cargo test -p rift-http-proxy --test corpus_replay
```

## Adding a fixture

Fixtures are numbered and append-only (`NN-name.json`, never renumbered). Add the next free number,
register it in `sdk-conformance/manifest.json`, and the engine gate enforces that it serves and its
`_verify` transcripts hold before it can ship. Seed material lives in the engine's
`tests/compatibility` and `examples/`.
