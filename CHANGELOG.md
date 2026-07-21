# Changelog

All notable changes to Rift are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This changelog was backfilled from git history and release tags; it summarizes user-facing
changes and omits internal refactors, CI, and test-only commits. See the git log for the full
record.

## [Unreleased]

### Added

- **Field-level `equals`-on-body predicates are now indexed by an automaton** (quamina), so
  "which of N field-equals-on-body stubs matches this request body" is one automaton pass
  instead of an `O(N)` scan of structural comparisons in the second matching stage. Measured in
  the shipped binary (#779) on stubs that share a path and differ only by a body field, where the
  dimension is what does the discriminating:

  | stubs sharing a path | without | with | Δ |
  |--:|--:|--:|--:|
  | 10 | 452,698 | 440,370 | −2.7% |
  | 100 | 317,912 | 436,007 | **+37.1%** |
  | 1000 | 84,210 | 404,809 | **+380.7%** |

  Without it throughput collapses as such stubs accumulate (453k → 84k); with it, it stays flat
  (440k → 405k). Matching results are unchanged — this is purely a prefilter (any predicate it
  cannot express — arrays, `null`, floats, `caseSensitive`/`keyCaseSensitive`, `except`/selector
  — safely falls through to full evaluation). It complements the `deepEquals`-on-body hash index.
  The capability is behind a default-on `quamina-matching` Cargo feature, droppable for minimal
  or FFI builds via `--no-default-features`; it costs ~0.49 MB of binary size. (#767, #777, #779)

- **Per-worker accept counters + runtime-topology bench support** (part of the RFC-712 gate,
  #746). `rift_accepted_connections_total{worker=…}` counts accepted connections per
  accept-loop slot — under `--runtime per-core` that is the worker index, so SO_REUSEPORT
  4-tuple skew is observable in production rather than inferred. The direct benchmark harness
  gained `--runtime {work-stealing,per-core}` with a topology self-report probe (a per-core
  request on macOS aborts instead of benching the fallback under a wrong label) and
  suffix-composed artefacts alongside `--allocator`.

- **Per-core topology now serves imposter traffic (experimental, RFC-712).** Under
  `--runtime per-core[=N]` every imposter port binds one SO_REUSEPORT listener per worker
  runtime and each accept loop runs pinned to its worker; the kernel spreads connections by
  4-tuple hash. All listeners for a port share one imposter, one connection tracker, one
  shutdown broadcast, and one backpressure semaphore — so delete/drain semantics and the
  global `RIFT_MAX_CONNECTIONS` cap are identical in both topologies, and creates are
  all-or-nothing (a port is never half-bound). Embedders get the same seam via
  `ServerBuilder::accept_runtimes` / `ImposterManager::with_accept_runtimes`. The default
  work-stealing topology is unchanged. (#745)

- **Opt-in per-core runtime topology (experimental, RFC-712).** `--runtime per-core[=N]` (or
  `RIFT_RUNTIME`) boots N single-threaded worker runtimes behind per-worker command channels,
  with the control plane (admin API, metrics, mutations) on a small multi-thread runtime, and
  `--runtime-affinity` optionally pins workers to cores. In this release the workers are
  topology plumbing only — imposter listener fan-out lands in a follow-up — and the default
  work-stealing runtime is completely unchanged. Linux-first by design: macOS falls back to
  work-stealing with a warning (its SO_REUSEPORT does not hash-balance accepts), Windows
  rejects the flag. The binary logs `Runtime topology: <mode>` at startup. (#744)

### Added

- **Opt-in `jemalloc` allocator feature for the server binary** (allocator bake-off, part of
  #717). `cargo build --release --no-default-features --features redis-backend,javascript,jemalloc`
  builds `rift-http-proxy` with tikv-jemallocator instead of the default mimalloc; when both
  allocator features are enabled (e.g. `--all-features`), mimalloc takes precedence. The binary
  logs `Global allocator: <name>` at startup, and the direct benchmark harness gained
  `--allocator {mimalloc,jemalloc,system}` — per-allocator builds, RSS sampling
  (`rss_mb_peak`/`rss_mb_end`), and suffixed result artefacts for three-way comparison. The
  default allocator is unchanged; any switch is gated on #717's pre-registered decision rule.

### Fixed

- **Recording-path throughput collapse once the journal hit its 10,000-entry cap.** A full
  journal evicts on every recorded request, and the per-eviction warning emitted one log line
  per request (87k+ lines/sec under load), serializing the whole recording path on the tracing
  subscriber's writer lock — measured at −29% to −55% RPS versus recording-off, and flat
  (non-scaling) throughput from 10 to 200 connections. The cap warning now fires once per
  fill-up (re-armed by deliberate deletions: `DELETE .../requests`, scoped clears, retention),
  and recording overhead drops to ~2% of throughput at saturation. Cap and eviction semantics
  (10k entries, oldest-first, cursor truncation) are unchanged. (#718)

### Added

- **HTTP connection-builder tuning knobs and optional accept-side connection backpressure**
  (env-tunable, applied to the imposter, proxy, admin-API, and metrics listeners). The hyper
  connection builders previously ran on defaults: a ~400 KB per-connection buffer and no
  header-read timeout. `RIFT_HTTP_MAX_BUF` (default 64 KB) now caps the per-connection buffer —
  bounding memory at high connection counts without affecting normal small mock requests — and
  `RIFT_HTTP_HEADER_TIMEOUT` (default 30 s) sets an explicit header-read timeout so a
  slow-header (slowloris) client can no longer hold a connection open indefinitely. Both are
  conservative defense-in-depth tightenings, on by default. `RIFT_MAX_CONNECTIONS` (default
  unlimited — today's behavior) optionally bounds concurrently-served connections with a
  per-listener semaphore acquired *before* accept, so overload is absorbed by the kernel backlog
  rather than collapsing into unbounded queueing; it never RST-storms clients.

- **The `match=` filter grammar on recorded traffic gains `method=` and `path=` exact-equality
  clauses.** Alongside `header:<Name>=<Value>` and `flow_id=<Value>`, `GET`/`DELETE
  /imposters/{port}/savedRequests` and the SSE streams (`GET /events`,
  `GET /imposters/{port}/savedRequests/stream`) now accept `match=method=<Verb>` (case-sensitive)
  and `match=path=<Path>` (the bare path; the query string is not compared). Clauses AND together
  and compose with `since=`. This unblocks downstream SDKs filtering a cursor tail server-side by
  method/path (rift-java#148, rift-scala#37). The grammar stays closed and exact — query-param and
  body predicates remain out of scope. **Contract note:** the `400` body for an unknown clause now
  enumerates the new forms (`unsupported match clause '…' (expected header:<Name>=<Value>,
  flow_id=<Value>, method=<Verb> or path=<Path>)`). Older engines still fail closed with a clear
  `400` on the new clauses, so SDKs gate on a minimum engine version rather than sniffing.

### Changed

- **TLS session resumption is now configured explicitly on every serving listener**, including the
  intercept listener. Rift previously relied on rustls' defaults; it now installs a session cache
  and a ring-based ticketer with roughly six-hourly key rotation. Clients that support resumption
  skip a full handshake on reconnect — a visible latency win for HTTPS imposters under short-lived
  connections — and resumption remains transparent to clients that do not. No configuration
  surface: this is on by default with no way to opt out (#725).

- **`deepEquals`-on-body matching is now indexed by a structural body hash** instead of being
  compared stub-by-stub. For the classic contract-test workload — hundreds of stubs on one
  path/method, each asserting one exact JSON body — selecting the matching stub drops from an
  `O(stubs × body)` scan of structural comparisons to a single `O(body)` hash probe. Matching
  results are unchanged; this is purely a prefilter. The index applies to default-mode `deepEquals`
  whose `body` is a JSON object or array; `caseSensitive`/`keyCaseSensitive`, a `jsonpath`/`xpath`
  selector, an `except`, a scalar `body`, or an expected body containing a string that is itself
  JSON all fall back to the existing full comparison (correctness is never affected, only pruning).

### Fixed

- **The quamina body-field dimension never reached the `rift` binary or the C-ABI.** It was
  default-on for `rift-mock-core`, but `rift-http-proxy` and `rift-ffi` take that crate with
  `default-features = false` and did not forward the feature — so the `#[cfg(not(...))]` no-op
  branch is what shipped, and every body predicate fell through to a full Stage-2 scan in the
  binary and in the C-ABI used by all four SDKs. CI stayed green throughout because the dimension's
  tests run inside `rift-mock-core`, the one crate where it *was* enabled. Both crates now forward
  it, and `scripts/verify-feature-propagation.sh` gates the invariant in CI: every default feature
  of `rift-mock-core` must be forwarded by, and default-on in, every crate that takes it with
  `default-features = false`. Matching **results** were never affected — the dimension is a pure
  prefilter — so this is a throughput fix, not a correctness one. The end-to-end win was
  subsequently measured in the shipped binary and is quoted under *Added* above (#779). (#777)

- **An imposter with a numeric or boolean header value was rejected with a `400`.** Mountebank
  tolerates non-string scalar header values — its own recorders routinely emit
  `"Content-Length": 124` and `"X-Flag": true` — and coerces them to their string form. Rift
  accepted neither, so a recorded Mountebank imposter replayed against Rift failed to load at all,
  with the whole document rejected rather than the one field. Numbers and booleans are now accepted
  and coerced to their string form, singly and inside multi-value arrays; `null`, objects, and
  nested arrays are still rejected (issue #754).

- **A systemic accept-loop failure could flood the log at the rate of the failure itself.** Every
  `accept()` error was logged at warn level, so a persistent condition — a process-wide fd
  exhaustion, for instance — produced one line per failed accept, drowning the log precisely when it
  was most needed and burning CPU on the write. Transient, per-connection errors
  (`ECONNABORTED`/`EINTR`/`ECONNRESET`) are now `debug!` — they are normal and the loop simply
  continues. A systemic error logs **once** on entry and once on recovery, reporting how many
  occurrences were suppressed in between, and the loop backs off exponentially (1 ms → 1 s) instead
  of spinning. No change to which connections are served (#768).

- **A stub whose configured `Content-Type` used a nonstandard casing (e.g. `CONTENT-TYPE`) with a
  JSON (non-string) body emitted two `Content-Type` headers.** The default-injection gate only
  checked the two literal casings `content-type` and `Content-Type`, so any other casing failed the
  check and Rift appended its own `Content-Type: application/json` alongside the configured header.
  The check is now case-insensitive (matching Mountebank, whose header handling is case-insensitive),
  so such stubs emit a single, configured `Content-Type`. Only stubs combining a nonstandard-cased
  `Content-Type` header with a JSON body were affected (issue #723).

- **A path-anchored stub could silently stop matching when its anchor contained non-ASCII text.**
  The Stage-1 path prefilter folded case with Unicode `to_lowercase`, while predicate evaluation
  folds with ASCII (`eq_ignore_ascii_case`). Unicode folding is length-changing and
  context-sensitive, so for `startsWith`/`contains` anchors the two disagreed: a stub with
  `startsWith: {"path": "/ΟΣ"}` was pruned from the candidate set for a request to `/ΟΣΑ` — which
  evaluation *would* have matched — and the request fell through to a later stub or to no match at
  all. The prefilter now folds exactly as the evaluator does, so the index and evaluation can no
  longer diverge. Only stubs whose path anchor contained non-ASCII characters were affected.

- **A request whose body fails mid-read now answers `400`, not `413`, and is no longer silent.** The
  imposter body reader funneled the size-cap breach and a genuine transport failure — a connection
  reset, a truncated chunked body — through one error arm that always answered `413` "Request body
  exceeds maximum size" and logged nothing. A dropped connection was reported to the client as a
  payload-size problem, with no server-side trace to correlate. The two are now distinguished: the
  cap breach is still `413` byte-identical, while a read failure is a `400` "Failed to read request
  body" (the client's transmission, not a server fault) and its cause is logged. Mirrors the
  admin-plane body reader (issue #546), which already drew this line.

- **The last imposter error doors that served plain text now serve the standard JSON envelope.** Nine
  doors — a debug-matching task panic and its timeout, plus the seven "Response build error"
  fallbacks that fire when a stub/inject/script/upstream header value is one the HTTP layer rejects —
  answered a bare string instead of the `{"errors":[{"code","message"}]}` envelope every other door
  uses (issues #682/#686/#687). They now serve the envelope with `Content-Type: application/json`,
  finishing that unification. **The debug-matching timeout also changes status from `500` to `504`**
  and gains the `x-rift-script-timeout` marker: it is a miss of the same `_rift.scriptEngine.timeoutMs`
  budget every other script deadline maps to `504` (issues #476/#499), so `500` contradicted that
  contract. Two doors stay plain text by design — the `Unknown fault` config diagnostic and the
  minimal internal-error last resort — since neither answers the envelope's audience.

## [0.14.0] - 2026-07-16

### Added

- **A `-static` image flavor: the same rift, on `FROM scratch`.** Published alongside the existing
  tags as `zainalpour/rift-proxy:<version>-static` and `:latest-static`, built from the musl release
  binaries the release workflow already produced. It contains the rift binary, a CA bundle and a
  passwd entry — nothing else. No package manager ever runs in it, so an image scanner finds no OS
  packages to report, which is the point: teams running rift in ephemeral test environments were
  patching a Debian userland that rift never uses. This is possible because rift's TLS is pure
  rustls pinned to `ring` with no OpenSSL anywhere, so the musl binaries are genuinely static. Two
  differences from the default flavor, both documented: there is no shell (use exec-form commands),
  and the musl builds omit the mimalloc allocator. Scripting and the Redis backend are present in
  both, and HTTPS upstream proxying works in both — the CA bundle is copied in.
- **`rift healthcheck`** — probes the admin API's `/health` and exits 0/1, reading `--host`/`--port`
  (so `MB_HOST`/`MB_PORT`) exactly as the server does. It exists so an image needs no shell and no
  curl to report its own health; `--url` probes something else, `--timeout` bounds it.
- **Published images now carry an SBOM and max-mode provenance, and are signed with cosign keyless.**
  The signing identity is the release workflow itself, so there is no key to distribute; `cosign
  verify` is documented under Deployment → Docker. Enterprise image-curation pipelines check exactly
  these, and an unattested image is what gets rejected at their gate.
- **The intercept listener and its rules can be declared in `--configfile`.** Imposters were
  declarative but intercept rules were not: they could only be installed at runtime over
  `POST /intercept/rules`, so every containerized intercept deployment needed a second "bootstrap"
  container to `curl` the admin API after boot. That sidecar exits `0` once it has posted the rule,
  which Kubernetes' `restartPolicy: Always` treats as a crash — the usual workaround is keeping a
  whole pod alive with `sleep` — and because `depends_on` ordering can't be gated on it, the system
  under test could start *before* the rule existed and hit the unmatched-host default. A config file
  may now carry an optional top-level `intercept` block (`{host?, port?, ca?, rules[]}`) alongside
  its `imposters`, so one declarative file brings up the listener with its rules already installed:
  `rift --configfile config.json`, no admin call and no sidecar. The rules are seeded *before* the
  listener binds, so there is no window in which it accepts traffic without them. The block reuses
  the `POST /intercept` body shape and the existing rule schema verbatim; `POST /intercept` and the
  FFI `rift_start_intercept` gain the same optional `rules` array, so any surface can start-and-seed
  in one call. Optional and additive: a config without the block, and any existing payload without
  `rules`, behave exactly as before. Supplying the block together with `--intercept-*` flags is a
  startup error rather than a silent precedence guess, and a rule using an `inject` predicate
  requires `--allowInjection` just as a config-file imposter's scripting surface does. The block is
  read from the `{"imposters": [...]}` wrapper form; writing one into a single-imposter document is
  a startup error naming the fix, never a block that silently does nothing. `POST /admin/reload`
  continues to apply imposters only, and now returns a `warnings` entry (and logs one) when the
  reloaded file carries an `intercept` block, so an edit to it never looks applied when it wasn't.

### Changed

- **The compiled-regex cache does lock-free reads and no longer flushes wholesale on overflow.** It
  was a `RwLock<HashMap>` taking a shared read lock on every cache hit — a lock on the matching hot
  path, per regex predicate per request — and on reaching its 1024-entry ceiling it `clear()`ed the
  entire cache, so a workload cycling just over the cap recompiled everything repeatedly. It is now
  a lock-free `papaya` map (hits are a wait-free guarded lookup, no lock) that evicts a bounded
  batch (~a quarter of the cap) on overflow instead of clearing, so the bulk of the working set
  stays hot across the boundary. Compile semantics and keying are unchanged.

- **The imposter port registry is now lock-free on every request-serving lookup.** It was a
  `RwLock<HashMap<u16, Arc<Imposter>>>`; the admin gateway (`/__rift/:port/...`) and embedded
  per-request dispatch resolved a port through that read lock on the hot path. Because the port
  keyspace is `u16`, it is now a fixed 65536-slot `ArcSwapOption` table (one slot per possible port,
  ~512 KB) — a lookup is a single wait-free atomic load, no hashing and no lock. Mutations
  (create/delete) are serialized by a small mutation mutex that preserves the previous
  check-then-insert duplicate-port semantics exactly. Admin listings (`GET /imposters`, metrics) now
  enumerate ports in deterministic ascending order (previously arbitrary hash order). No admin API
  shape changed.

- **XPath/JSONPath selectors are compiled once and the XML DOM is parsed once per request, not
  per predicate per stub.** Rift's slowest scenario was XPath, because every XPath predicate
  evaluation re-parsed the whole request body into a DOM and recompiled the XPath expression — so N
  XPath stubs sharing a path cost N DOM parses + N compilations per request; JSONPath likewise
  re-parsed both the body and the selector each time. Now the request body's XML DOM is parsed at
  most once per request and shared across every stub/predicate that evaluates it; compiled XPath
  expressions are cached per thread (the `sxd` XPath type is not shareable across threads) and
  compiled JSONPath selectors in a process-wide bounded cache; and JSONPath predicate matching
  reuses the request body already parsed once for `deepEquals` (#290). Matching semantics are
  unchanged — the same engines against cached artifacts. The parse-/compile-once guarantees are
  asserted directly by test counters.

- **Path `startsWith`/`contains`/`endsWith` stubs are matched in a single Aho-Corasick pass, and
  `endsWith` is now indexed at all.** The Stage-1 prefilter previously walked a linear bucket per
  distinct prefix/substring literal, so the prefilter's own cost grew with the number of literal
  stubs even when one matched; `endsWith` was never indexed and every such stub was evaluated in
  full. All three literal kinds on `path` are now compiled into two multi-pattern automata (one per
  case class), and one overlapping pass answers which literals match — `startsWith`/`endsWith` are
  distinguished by match position, so no separate automata are needed. On a corpus of
  `startsWith`-anchored stubs matching the last, per-request matching went from ~1.69 µs to ~197 ns
  at 1000 stubs (~8.5x); exact-path matching is unchanged. Matching *semantics* are unchanged: the
  index only ever over-approximates and full predicate evaluation remains the single source of
  truth. A pattern set the automaton build rejects falls back to full evaluation and warns.

- **Path-regex stubs are matched in a single pass, instead of one regex execution per stub.** A
  `matches` predicate on `path` was previously invisible to the Stage-1 prefilter, so every regex
  stub was a candidate for every request and each ran its own compiled regex — the cost grew
  linearly with the number of regex stubs. Those patterns are now compiled into two multi-pattern
  automata (one per case class), and one pass over the request path answers which of them match.
  On a corpus of regex-anchored stubs matching the last, per-request matching went from ~13.0 µs to
  ~211 ns at 100 stubs (~62x) and from ~211 µs to ~487 ns at 1000 stubs (~433x) — the cost is now
  effectively flat in regex-stub count rather than linear. Matching *semantics* are unchanged: the
  index only ever over-approximates and full predicate evaluation remains the single source of
  truth. Patterns the multi-pattern build rejects (an oversized or unparseable pattern) simply keep
  the previous per-stub behaviour, and say so in a `warn` naming the stub.

- **Stub matching prunes candidates on request *method*, not just path.** The Stage-1 prefilter is
  now a multi-dimensional candidate-bitset index (the Lucent Bit Vector technique from packet
  classification): each dimension answers which stubs a request attribute cannot rule out, and the
  per-dimension bitsets are intersected. A `POST`-anchored stub is therefore no longer a candidate
  for every `GET`. On a 1000-stub corpus sharing one path and partitioned 6 ways by method,
  matching went from ~33.4 µs to ~12.9 µs per request (~2.6x). Matching *semantics* are unchanged:
  the index only ever over-approximates, and full predicate evaluation remains the single source of
  truth (a randomized differential test asserts the indexed path returns exactly what an
  exhaustive linear scan does).

- **`Imposter::stubs` is no longer a public field.** It and the internal `stub_index` were merged
  into one atomically-swapped snapshot so the match hot path takes a single wait-free load and a
  torn (stubs *N*, index *N+1*) read is unrepresentable. The stub-reading API is unchanged —
  `get_stubs`, `get_stub`, `get_stub_by_id`, `stub_count` and the admin/FFI surfaces all behave
  exactly as before; only direct field access by an embedding Rust consumer is affected.

- **Every image base is now digest-pinned** (`image:tag@sha256:...`), with Dependabot owning the
  bumps. `rustlang/rust:nightly` floats daily, so builds of the same commit were not reproducible.

### Fixed

- **Two requests differing only in their query string no longer share one cached script decision.**
  The decision-cache key was built from `Uri::path()`, which excludes the query — but scripts read
  the query as `ctx.request.query`. So `?scenario=timeout` and `?scenario=none`, identical in every
  other respect, produced the same key, and the second request was served **the first one's fault
  decision**: silently, for up to the cache TTL. Query-driven scenarios are an ordinary way to drive
  a fault-injection proxy, and this affected the default configuration (cache on, 300s TTL) for any
  deployment not using flow state. This is the same defect as the non-JSON-body collision below, one
  component over: the key must cover every request-varying input the memoised script can observe —
  headers came under that rule earlier, the body did too, the query never had. The query now enters
  the key on its raw spelling, so `?a=1&b=2` and `?b=2&a=1` are two entries — deliberate, since the
  only cost is a cache miss, where keying on the parsed form could hand one request another's
  decision. **Breaking (embedders only):** `CacheKey::new` takes a `query: Option<&str>` after
  `path`.

- **Two requests with different non-JSON bodies no longer share one cached script decision.** The
  decision-cache key hashed only the *parsed* body, and every body that is not JSON — every
  protobuf, gzip or image upload, every text payload, every malformed body, and the empty body —
  parsed to `null`. So any two of them with the same method, path, keyed headers and rule produced
  an identical key, and the second request was served the **first one's fault decision**: silently,
  for up to the cache TTL, with nothing logged to correlate. Scripts read the body as
  `ctx.request.raw_body`, so this was the cache discarding an input the memoised script can branch
  on. The body now enters the key as its raw bytes whenever it is not JSON, in a separate hash
  domain from parsed JSON — so a JSON `null`, an empty body and a binary body are three distinct
  keys by construction. JSON bodies keep their structural key (formatting and key order still do
  not split it), identical payloads still hit the cache, and the non-JSON path is *cheaper* than
  before: hashing bytes beats walking a JSON tree. The script-visible contract is unchanged —
  `ctx.request.body` is still `null` for a non-JSON body, with the bytes on `raw_body`/`mode`.
  Affects the default configuration (cache on, 300s TTL) for any deployment not using flow state.
  **Breaking (embedders only):** `rift_mock_core`'s `CacheKey::new` now takes a `CacheKeyBody<'_>`
  (`Json(&Value)` / `Raw(&[u8])`) in place of `&serde_json::Value`. In-process callers building
  cache keys directly must wrap the argument; no configuration or wire format changes.

- **A failed proxy now logs *why*, instead of the same opaque line for every cause.** An imposter
  whose upstream could not be reached logged `Proxy request failed: Failed to send proxy request to
  <url>` — the outermost context and nothing else, because the error was formatted with `{}`, which
  renders only the top of an `anyhow` chain. A DNS failure, a TLS certificate rejection, a refused
  connection and a timeout were therefore indistinguishable in the log, on a 502 whose whole job is
  to say the upstream did not answer. The cause was never lost by the code — it rides on the error
  from the moment it is captured — only by the format specifier; the log now renders the whole chain
  (`dns error: failed to lookup address information: Name does not resolve`). The same fix applies
  to the `defaultForward` upstream error and to the `inject` and script-execution failures, which
  dropped their chains the same way. **The client-facing body deliberately does not change in this
  respect:** a cause chain can name internal hosts and resolver detail, so the 502 still carries
  only the outermost context — the chain's audience is the operator, who now has it.

- **Proxy and `defaultForward` 502 bodies are valid JSON, in the same envelope as every other proxy
  error.** Both hand-built their body by interpolating the error into a JSON string literal, so any
  message containing a `"` produced a body the client's decoder rejected — the defect class issue
  #611 swept elsewhere. They now go through the crate's canonical Mountebank error builder.
  **The body shape changes** from `{"error": "..."}` to the standard
  `{"errors":[{"code":"502","message":"..."}]}`, and now carries `Content-Type: application/json`,
  which it previously omitted. This completes the unification 0.13.6 claimed — that release moved
  the standalone proxy paths onto the one envelope but missed this imposter door, leaving the same
  failure answering in two shapes depending on which door the request came through. Status and the
  `x-rift-imposter` / `x-rift-proxy-error` / `x-rift-default-forward-error` markers are unchanged.

- **Every remaining imposter error body is valid JSON, in the one standard envelope.** A script that
  failed with a quote in its message — `throw new Error('expected "ready", got "pending"')`, which is
  how error messages ordinarily read — produced a **body the client could not decode**. Ten doors
  built their JSON by interpolating the message into a string literal, so the quote closed the string
  early: the reply was a 500 whose payload died in the caller's parser, replacing a legible script
  error with a JSON syntax error. They all now go through the same builder the rest of the crate
  uses, which escapes via serde. The doors: script execution (500) and its timeout (504), template
  rendering, the three `strictBehaviors` failures (decorate, shellTransform, binary decode), a
  disabled imposter (503), an over-size request body (413), an unresolved `file:`/`ref:` script, and
  the debug-response serialize fallback. **The body shape changes** from `{"error": "..."}` to
  `{"errors":[{"code":"<status>","message":"..."}]}` — the same move the previous entry made for the
  proxy doors, finishing the class: no imposter door hand-builds JSON now. Statuses and every marker
  header (`x-rift-script-error`, `x-rift-script-timeout`, `x-rift-decorate-error`,
  `x-rift-template-error`, `x-rift-imposter-disabled`) are untouched, and each envelope's `code` is
  its own response's status — including the decorate door, whose status varies with whether it timed
  out. As with the proxy doors, the client still gets only the error's outermost context; the cause
  chain stays in the server log.

- **Every imposter error door that serves the JSON envelope now declares `Content-Type:
  application/json`.** The previous two
  entries unified what these doors *say*; they left the same envelope typed on one path and untyped
  on another. The proxy doors and the admin plane announced `application/json`, while every imposter
  door omitted it — so a client branching on content-type, or a strict proxy or logging middleware
  that requires it, treated one shape as two depending on which door answered. The doors: the
  response and predicate `inject` errors (400) **and their timeouts (504)**, script execution (500)
  and its timeout (504), template rendering, the three `strictBehaviors` failures (decorate,
  shellTransform, binary decode), a disabled imposter (503), an over-size request body (413), and an
  unresolved `file:`/`ref:` script. Statuses, bodies and every marker header are unchanged — this
  adds a header and nothing else. Doors that do **not** serve the envelope keep their typing — the
  empty-body 502 fault passthrough, and the plain-text replies (`Unknown fault`, the debug-matching
  failures), are untouched, since labelling any of them `application/json` would be a lie a client
  would act on.

### Security

- **The container images no longer install `curl`** ([CVE-2025-10148]).
  curl was in the image for exactly one reason — to run the `HEALTHCHECK` line — and that line now
  execs the binary's own `rift healthcheck` instead. The image's intent was always "just the rift
  binary"; nothing else in it used curl. Downstream consumers were carrying a medium-severity
  advisory in their test infrastructure on account of a probe.

[CVE-2025-10148]: https://nvd.nist.gov/vuln/detail/cve-2025-10148

- **`POST /intercept/rules` now obeys `--allowInjection`.** An intercept rule's predicates are
  evaluated on every intercepted request, so an `inject` predicate is executable JavaScript — but
  this door never asked the `--allowInjection` gate. The identical predicate was refused with `400`
  by `POST /imposters` and executed by `POST /intercept/rules`, on a server started without the
  flag and on an admin port that is unauthenticated unless `--apikey` is set. Since admin access is
  deliberately *not* supposed to grant code execution — that is precisely what the flag gates —
  this was a privilege escalation past rift's own stated boundary. Issue #612 swept every door that
  admits config through one classifier; this is the door it missed. All rule doors now ask it:
  `POST /intercept/rules`, the `rules` array on `POST /intercept`, and the `--configfile`
  `intercept` block. The refusal is atomic and sees through `not`/`or`/`and` nesting — a batch
  containing one gated rule stores none of it, and a refused start binds no listener.
  **Behaviour change:** a rule with an `inject` predicate now needs `--allowInjection`, and gets the
  same `400` and remedy message every other door gives. `serve`/`forward` actions carry no script
  and are unaffected.

## [0.13.6] - 2026-07-15

### Fixed

- **Binary request bodies are no longer silently corrupted when recorded or handed to scripts.**
  Request bodies went through `String::from_utf8_lossy`, which replaces every invalid byte with
  U+FFFD — so a protobuf, gzip or image upload was recorded as something the client never sent,
  irreversibly and with no error. Rift records and replays real traffic, so reading a recording back
  or round-tripping it silently produced wrong bytes. A non-UTF-8 body is now base64-encoded and
  marked `"_mode": "binary"` on the recorded request, mirroring how binary *response* bodies have
  always been represented; scripts get the base64 body plus `isBinary` on `ctx.request`. This is
  additive: `_mode` is absent for text bodies, so existing recordings and all-text traffic are
  unchanged. The fault-injection proxy path had the same bug and is fixed too. Where rift genuinely
  cannot classify the body (the `decorate` and predicate-`inject` paths), `isBinary` is absent
  rather than `false` — a script can tell "text" from "unknown" instead of being told something
  untrue.

- **Intercept-rule body predicates no longer evaluate against corrupted binary payloads.** The
  TLS-intercept forward-proxy path ran the intercepted request body through
  `String::from_utf8_lossy` before rule matching, replacing every invalid byte with U+FFFD — so a
  body predicate on binary traffic (protobuf, gzip, an image upload) matched or failed to match
  against garbage the client never sent. A non-UTF-8 body is now matched against its standard
  base64 encoding, the same convention used for binary recorded requests and binary responses;
  write the predicate against the base64 string. Text/JSON bodies are matched as-is, unchanged.
  Forwarding was never affected — it always relayed the raw bytes. **Behavior change:** an
  intercept-rule body predicate that deliberately matched the U+FFFD-mangled form of a binary body
  must be rewritten against the base64 encoding.

- **Query-parameter *names* are now percent-decoded everywhere, so `?first%20name=bob` matches a
  predicate on `first name` on every path.** Rift has four query/form parsers, and two of them
  decoded only the value, leaving the key raw — so the same request got a different answer
  depending on which path evaluated it: the imposter's predicate matching saw the key `first name`
  while `deepEquals` predicates, rule matching (`_rift.match.query`), response templates, and the
  request context handed to behaviors all saw `first%20name` and failed to find it. Mountebank
  decodes both key and value (Node's `querystring.parse` unescapes keys), so the raw-key paths were
  also a compatibility divergence. An undecodable key (e.g. `%FF`) passes through raw, consistent
  with Rift's decode contract for values. **Behavior change:** a config that relied on matching the
  raw encoded key name (e.g. a matcher literally named `first%20name`) will no longer match; name
  the matcher with the decoded key instead. Two parameters whose names differ only by encoding
  (e.g. `a%2Bb` and `a+b`) now decode to the same key and merge under each path's existing
  duplicate-key rule, as they already did on the imposter's matching path.

- **`"wait": {"inject": "function(){...}"}` now works — it was silently doing nothing.** The
  object spelling of a function wait is what `docs/features/fault-injection.md`, the shipped
  `examples/latency-testing.json`, and the SDKs all use, but the engine's `WaitBehavior` had no
  variant for it. Worse than a load error: `_behaviors` is parsed once into a cache and the parse
  failure was swallowed, so such a config started cleanly and served requests with the **entire
  `_behaviors` block dropped** — no latency, no `repeat`, no error, no log. The shipped example's
  `/random-latency` stub has been answering with no delay. Both spellings are now accepted and
  execute identically (same sandbox, same 60s cap); Rift preserves whichever you wrote when you
  read the imposter back. The bare string remains the Mountebank-compatible form; the object form
  is a Rift superset like `{"min","max"}`. `rift-lint` accepts it too — it previously flagged
  Rift's own example as an error. A `_behaviors` block that still fails to parse is now logged at
  error level instead of vanishing.

- **A failed response build now answers `500`, not `200` with the words "Internal Server Error".**
  The shared terminal fallback behind every serving-path response builder used `Response::new`,
  which defaults to status **200** — so a builder failure (an invalid header name or value, e.g.
  from a proxied upstream, an inject, or a script) served an error string under a success status,
  and the client's decoder was the first thing to notice. The fallback now sets a real `500` and
  logs at error level. Six previously-silent builder fallbacks on the serving path now log the
  cause instead of swallowing it.

- **Proxy error responses are valid JSON and carry a correct status.** The proxy's error helper
  interpolated the message straight into a JSON string literal, so any message containing a quote
  produced a body that failed in the client's parser; it also took an unvalidated `u16` status, and
  a code HTTP cannot represent fell into a fallback that answered **200**. It now delegates to the
  same Mountebank-shaped error builder the rest of Rift uses (`{"errors":[{"code","message"}]}`),
  escaping the message properly, and an unrepresentable status is a logged `500`. Note the body
  shape of proxy-generated errors (e.g. `Bad Gateway`) changes from `{"error": "..."}` to the
  Mountebank envelope, matching Rift's other error responses. All proxy modes now emit that one
  envelope; previously the streaming and recording paths hand-built their own.

- **`proxyAlways` no longer records a duplicate stub when a predicate matches on several fields.**
  Stub dedup compared *serialized* predicates, but a predicate's operands are maps that serialize
  in iteration order, so two semantically identical predicate sets reliably produced different
  JSON strings. A `predicateGenerators: [{"matches": {"method": true, "path": true, "query":
  true}}]` therefore appended a new stub per recorded request instead of merging responses into
  the existing one. Dedup now compares the predicates structurally.

- **A query or form value that is not valid percent-encoding is passed through raw instead of
  being blanked.** Five decode sites — the behaviors request context, both `parse_query_string`
  helpers, and form-body parsing — turned an undecodable sequence into an **empty string**, so a
  predicate matched against `""` rather than the text the client actually sent, and an undecodable
  *key* collapsed distinct parameters into a single `""` entry that comma-joined unrelated values.
  They now pass the raw value through, which is what every other decode site in Rift already did.

- **Debug-mode responses report a serialization failure as `500`.** An `X-Rift-Debug: true`
  request answered `200` carrying an error string if the debug payload failed to serialize, with
  no log.

### Security

- **`allowInjection` is now enforced on the FFI's `configFile` door (`rift_serve_admin`).** The
  embedded admin plane honoured `allowInjection` for imposters submitted through it, but the
  `configFile` option loaded and executed a scripted imposter regardless — so an embedding host
  that passed `allowInjection: false` and pointed Rift at a config file it did not fully control
  (ops-provisioned, mounted, edited out-of-band) still ran `inject`/`decorate`/`shellTransform`/
  JS-function `wait`. This was the last door left open by the previous entry, which closed the
  CLI's `--configfile`/`--datadir`. A scripted `configFile` now fails the serve outright — `NULL`
  plus a `rift_last_error` naming the offending ports — so nothing is applied.

  **Unchanged, and deliberately so:** in-process config from the embedding host — the inline
  `config` option, `rift_apply_config`, `rift_create_imposter` — remains **ungated**. The gate's
  subject is a config *document that crossed a trust boundary*, not the host: a caller holding the
  C-ABI can already execute code in the process, so gating its own JSON would restrict nobody while
  breaking hosts that legitimately drive script imposters over the C-ABI. The trust boundary is now
  documented in `docs/embedding/ffi.md`.

- **A function `wait` requires `--allowInjection` in both spellings.** The `--allowInjection`
  admission gate classified only the bare-string function wait as a scripting surface, so the
  newly-accepted `{"inject": ...}` form would have executed JavaScript with injection disabled.
  Both spellings are one capability and now gate identically. The gate also **fails closed**: a
  `_behaviors` block it cannot parse is treated as scripted rather than admitted as
  "no script surface" — previously its safety depended on the executor's parser failing in
  lockstep, an unwritten coupling this release's new wait variant would have broken.

- **`--allowInjection` is now enforced on `--configfile`, `--datadir`, and `POST /admin/reload`.**
  The gate only ever guarded the admin API, so the same document got two different security
  answers depending on the door it came through: `examples/latency-testing.json` (a JS-function
  `wait`) was correctly refused when POSTed without `--allowInjection`, and loaded and executed
  when passed to `--configfile`. This mattered most for `--datadir`, which is **not**
  operator-authored — its `{port}.json` files are persisted from admin-API writes, so running once
  with `--allowInjection` and restarting without it kept executing the persisted scripts: the gate
  failed open across restarts. `POST /admin/reload` was ungated and network-reachable. All doors
  now ask the same classifier the admin API uses (`inject` response/predicate,
  `predicateGenerators.inject`, `decorate`, `shellTransform`, a non-numeric `wait`,
  `_rift.script`), with per-door semantics: `--configfile` **aborts startup**, naming the file and
  every offending port at once; a gated `--datadir` file is **skipped** and named in the existing
  startup skip summary, so one leftover file cannot brick the rest; `POST /admin/reload` returns
  **`400 invalid injection`** before applying anything, leaving running imposters untouched.
  Mountebank refuses injection in a config file too — see the divergence note in the compatibility
  matrix: mb logs the failed load and stays up, Rift fails fast on `--configfile`.

### Added

- **Admin SSE stream — push request tails instead of polling.** `GET /events` (and the
  handle-scoped alias `GET /imposters/{port}/savedRequests/stream`) streams recorded requests and
  imposter lifecycle changes as Server-Sent Events, so an SDK request tail (`ZStream`/`fs2.Stream`,
  Go channels, async iterators) gets pushed events instead of paying a poll interval. Filterable by
  `types=requests,lifecycle`, `port=`, and the same `match=` clauses as `savedRequests`; gated by
  the admin API key like every other admin route; heartbeat comments keep idle streams alive.
  Publishing is a no-op when nobody is subscribed, so the request hot path is untouched unless a
  client is connected. Lossy-but-loud under backpressure: a slow consumer gets a `lagged` event
  rather than stalling the engine. Each `request` event carries its journal `index`, so a client
  that lagged or reconnected reconciles with `savedRequests?since=<index>` instead of re-polling the
  whole journal. Older engines return `404`, which is the SDK's capability probe — polling remains
  the supported fallback and the source of truth (v1 does not replay).
- **`savedRequests` cursor — tail an imposter's recorded requests without re-fetching the journal.**
  `GET /imposters/{port}/savedRequests?since=<index>` now serves only the requests newer than a
  cursor, so an SDK request-tail costs O(new entries) per poll instead of O(journal) with
  client-side dedupe. Every recorded request gets a stable, 1-based, per-port index; the cursor
  rides in the `x-rift-next-index` response header (pass it back verbatim as the next `since`), and
  the response body stays the same bare JSON array, so existing clients and Mountebank compatibility
  are unaffected. `since` composes with the existing `match=` filters, and the cursor always
  advances past entries a filter rejected — a filtered tail never re-scans. Indices survive
  `DELETE savedRequests` and scoped clears (later entries simply get larger indices), so a cursor
  held across a clear stays valid. `x-rift-truncated: true` appears only when the 10k retention cap
  evicted entries you had not seen yet — the signal to re-baseline. Absence of `x-rift-next-index`
  is the capability probe: older engines and custom `RequestJournal` backends without stable indices
  serve the full list and are polled exactly as before. The `RequestJournal` trait gains
  `read_since`/`record_indexed` as default methods returning "unsupported", so existing embedder
  implementations compile and behave unchanged.

## [0.13.5] - 2026-07-12

### Fixed

- **Deleting an imposter now fully tears it down before the call returns.** `DELETE /imposters[/{port}]`
  (and the FFI `rift_delete_imposter`/`rift_delete_all`, `PUT /imposters` reload, and `apply_config`
  deletes) previously signalled shutdown fire-and-forget and returned immediately, so the old
  imposter's listener could linger and its established keep-alive connections keep answering from the
  previous generation's state. Combined with `SO_REUSEPORT` (Linux) and client connection pools, an
  immediate same-port re-create could be served the deleted imposter's mid-cycle response (observed as
  a flaky first-request status on embedded conformance runs). Delete now awaits full teardown — the
  accept loop ends (listener unbound) and in-flight connections drain within a bounded window — so
  once it returns the old generation can no longer serve a byte and a re-create on the same port is
  race-free.

## [0.13.4] - 2026-07-12

### Added

- **Intercept CA: inline PEM input and a generate-and-return bootstrap mode.** `POST /intercept`
  (and the FFI `rift_start_intercept`) now accept the CA as inline PEM bytes — `caCertPem` /
  `caKeyPem` — in addition to the existing `caCertPath` / `caKeyPath` file pair, so an SDK can hand a
  containerized engine its CA over the admin API without a filesystem mount. The two source pairs are
  each both-or-neither and mutually exclusive. Setting `"returnCaKey": true` (valid only when no CA
  source is supplied) has Rift mint a fresh CA and return **both** its cert and key once in the `201`
  response, so a caller can persist and redistribute a shareable trust anchor instead of pre-making
  one with `openssl`. The CLI/env gains inline `RIFT_INTERCEPT_CA_CERT_PEM` / `RIFT_INTERCEPT_CA_KEY_PEM`
  (mutually exclusive with the file flags). **Security:** the returned `caKeyPem` is CA private-key
  material — it is returned once (never by `GET /intercept`), only when Rift generated the CA
  (combining `returnCaKey` with a supplied CA is a `400`, closing a filesystem-exfiltration path),
  and should be transported over the `--apikey`-gated admin plane and treated as a secret. Additive
  and feature-detectable (an older engine's `deny_unknown_fields` rejects the new fields with a
  `400`); the C-ABI contract version is unchanged.
- **`rift_abi_version()` — a queryable C-ABI contract version for SDK compatibility gating.** The
  new FFI symbol returns the C-ABI contract version (`uint32_t`, currently `2`), bumped only on a
  breaking ABI change and never on additive or bugfix releases. SDKs can now gate on the ABI
  contract they bind against instead of `rift_build_info`'s release/marketing `version`, which reads
  as the workspace placeholder (`0.1.0`) on locally-built engines and made compatibility floor
  checks misfire. Adopted behind feature detection: gate on the symbol when present, fall back to
  probing the symbol set when absent.

## [0.13.3] - 2026-07-12

### Fixed

- **The TUI imposter-list recording indicator now reflects real state.** The default
  `GET /imposters` list summary omitted `recordRequests`, so the TUI (which deserializes a missing
  field to `false`) always drew the indicator as not-recording regardless of the imposter's actual
  configuration. The list summary now carries `recordRequests`, sourced from each imposter's config,
  alongside the existing `stubCount`/`enabled` fields.
- **Response cycling (`repeat`) no longer serves a stale branch to a zero-latency next request.**
  The per-imposter response cursor advanced with `Relaxed` atomic ordering, so a strictly-sequential
  client that issued its next request with near-zero latency — reached via a different worker thread
  on a loaded in-process (embedded) runtime — could observe the pre-advance cursor and be served the
  previous response (e.g. a third `503` where a `repeat: 2` stub should have crossed to `200`). The
  cursor now advances with `SeqCst`, matching the ordering the sibling per-imposter cross-request
  state already uses, so the advance is published before the response is returned.
- **`PUT /imposters` no longer loses imposters on a partial failure.** The handler deleted every
  running imposter and then recreated the payload's set, merely logging any create failure — so a
  single bad imposter (or a transiently-held port) returned `200` with a silently smaller set, the
  previous imposters already gone. It now reconciles toward the payload with the same engine as
  `POST /admin/reload`: the whole set is validated before anything is touched (an invalid payload is
  a `400` with the running imposters unchanged), and residual per-port apply failures return a `500`
  whose body reports what failed and what did apply. (Behavior note: an imposter whose config is
  unchanged now keeps its runtime state — recorded requests, response cycling — instead of being
  torn down and recreated; `DELETE /imposters` first if a full reset is wanted.)
- **The TUI imposter list refreshes with one request instead of N+1, and no longer mis-renders a
  transiently-failing imposter as "Disabled / 0 stubs".** Every ~1s refresh listed the imposters and
  then fetched each one's detail sequentially just to fill in its stub count — 51 serial round-trips
  per tick with 50 imposters — silently dropping any per-imposter failure. `GET /imposters` now
  carries `stubCount` per entry (alongside the existing `enabled`), and the TUI renders the list
  straight from that single response.
- **Script-pool `queue_depth` / `active_tasks` metrics no longer leak on a timed-out script.** Both
  gauges were incremented per request but only decremented on the success path, so every script that
  hit its execution timeout (the exact case the pool bounds) permanently inflated them — the metrics
  drifted monotonically wrong under sustained load with slow scripts. Both counters are now released
  on every exit path (success, timeout, cancel) via an RAII guard.
- **`rift lint --fix` no longer corrupts valid multi-value header arrays.** The fixer rewrote every
  array-valued response header into a single comma-joined string and silently dropped any non-string
  element, so a valid `"Set-Cookie": ["a=1", "b=2"]` (a legitimate multi-value header since #238)
  was clobbered into one header with different runtime semantics — and a fully valid file could be
  rewritten just because a *different* file in the run had errors. `--fix` now leaves string-only
  arrays untouched and, for an array that genuinely contains a non-string element, stringifies the
  offending elements in place (preserving the array) instead of joining and dropping.
- **The TUI no longer panics on imposter names or paths containing multibyte UTF-8.** Several
  truncation sites guarded on byte length but sliced at a byte index, so a name/scenario/recorded
  path with multibyte characters (e.g. `日本語サービス` from imported JSON or proxy recordings)
  panicked with "byte index is not a char boundary" inside the render loop, tearing down the
  terminal (often leaving it in raw mode). Truncation is now char-based via a single shared helper.
- **Creating an imposter no longer silently succeeds when it can't be persisted to `--datadir`.** The
  create path wrote the config in a fire-and-forget task that only logged failures, so a datadir
  write error returned `201 Created` to the caller and the imposter then vanished on restart. Create
  now persists synchronously and, on failure, rolls back the in-memory imposter and returns a
  `503` (`ImposterError::PersistError`) — matching the durability contract already used by stub
  mutations (#173).
- **`rift lint` no longer executes JavaScript while syntax-checking it.** The `javascript`-feature
  validator ran scripts via the JS engine, so an inject/decorate body containing a loop (e.g.
  `while (true) {}`) hung the linter, and any engine error whose message lacked `SyntaxError`/
  `unexpected` was silently treated as valid. It now parses without executing and reports every
  syntax error.
- **`rift lint` no longer flags valid anonymous `async function`/generator inject bodies as syntax
  errors.** The `javascript`-feature validator wrapped a plain `function (…)` expression so Boa
  could parse it, but its detection missed the `async function (…)` and `function* (…)` forms, so
  those valid inject/decorate bodies were parsed as nameless declarations and mis-reported as
  errors. The detection now recognises the `async` prefix and the generator `*`.
- **Concurrent requests to a mixed-type response array no longer serve the wrong branch or a bogus
  empty 200.** Dispatch classified the response with a non-advancing peek and then advanced the
  cycler in a separate step, so under load a concurrent request could move the shared cursor between
  the two and, e.g., serve an empty `x-rift-no-match` 200 where a `proxy` or `is` response was
  intended. The cycler is now advanced exactly once per request and dispatched on the returned
  response. (Behavior note: the cursor now advances even when proxy/inject/script handling fails —
  a shared cursor can't be safely un-advanced under concurrency — where a failed handling previously
  left the cursor for the next request to retry.)
- **Flow-level `ctx.state.ttl(seconds)` no longer revives expired keys on the in-memory backend.**
  The positive-TTL branch re-stamped every entry in the flow — including entries already past their
  expiry that the amortized sweeper hadn't reaped yet — resurrecting them so a later `get`/`exists`
  saw them as live. It now drops already-expired entries before extending the survivors (and clears
  the flow map if that leaves it empty), matching the Redis backend, where expired keys are simply
  gone and never revived.
- **Per-key `ctx.state.ttl(key, seconds)` no longer leaks empty flow maps on the in-memory backend.**
  When a positive TTL was set on a key that had already expired and was the flow's last entry, the
  branch cleaned up the key but left the now-empty flow map in the store — so repeated calls across
  many flow ids grew the store without bound. It now drops the empty flow map, matching the sibling
  delete / `set_ttl` / sweep paths (issue #483).
- **The proxy no longer panics at startup when the native root certificate store can't be loaded.**
  `create_http_client` called `.expect(...)` on `with_native_roots()`, so running in a minimal or
  distroless image without `ca-certificates` aborted the process; it now returns the error so the
  server fails with a diagnostic instead of a panic.
- **`proxyOnce` no longer wedges when a client disconnects mid-proxy.** The pending claim taken when
  a request begins forwarding was only cleared once the response was recorded, so a client that
  disconnected before the upstream responded left the request signature stuck "pending" —
  subsequent matching requests got neither a proxy nor a recorded reply until a later completed
  request happened to self-heal it. The claim is now released if the forward is cancelled before it
  records.
- **A panic in the Redis flow-state backend no longer cascades into a repeating-panic storm.** Each
  pooled connection is a `Mutex<Connection>` that was accessed with `.lock().unwrap()`; one panic
  while a lock was held poisoned that mutex, so every later access panicked too, and the pool never
  discarded the connection (`is_valid`/`has_broken` panicked or reported it healthy). Poisoned locks
  are now recovered, and a poisoned connection is reported broken so the pool evicts it.

### Security

- **The intercept rule store is now capped at 10,000 rules, returning `429 Too Many Requests`.**
  `POST /intercept/rules` appended to an unbounded `Vec`, so a client could grow it without limit —
  exhausting memory and linearly slowing every intercepted request's rule-match scan. Additions past
  the cap are now rejected (a batch that would exceed it is rejected in full), bounding both. The
  same guard covers the FFI `rift_intercept_add_rules` path.
- **Admin API request bodies are now capped at 64 MiB, returning `413 Payload Too Large`.**
  `collect_body` previously buffered an entire request body into memory with no limit; since the
  admin plane binds `0.0.0.0` and `--apikey` is optional, an unauthenticated client could OOM the
  process with a multi-gigabyte `POST`. Every admin write handler funnels through the capped path.
- **Admin API key comparison is now constant-time.** The bearer-token check short-circuited at the
  first differing byte, leaking the configured `--apikey` to a timing side-channel; it now uses a
  constant-time comparison.
- **TLS-intercept per-SNI leaf cache is now bounded (LRU, 1024 entries).** The SNI on the intercept
  listener is attacker-controlled; the cache previously grew without limit and minted a fresh
  keypair per unique SNI, so a flood of unique names was a memory- and CPU-exhaustion vector. Old
  leaves are now evicted instead of accumulating.

## [0.13.2] - 2026-07-11

### Added

- **Per-key flow-state TTL and flow invalidation** (`ctx.state`): scripts can now set a single key's
  TTL with `ctx.state.ttl(key, seconds)` (returns `true` if the key existed, `false` if absent;
  `seconds <= 0` deletes it, matching Redis `EXPIRE`) alongside the existing flow-level
  `ctx.state.ttl(seconds)`. New `ctx.state.clear()` removes every key in the flow, and a new admin
  route `DELETE /admin/imposters/:port/flow-state/:flow_id` clears a whole flow (the test
  arrange/teardown tool). Both rhai and JavaScript engines expose the same surface.

- **Probabilistic TCP faults** (`_rift.fault.tcp`): the fault now accepts an object form
  `{ "probability": 0.1, "type": "CONNECTION_RESET_BY_PEER" }` alongside the existing bare string,
  so a connection reset can be made to fire only some of the time (e.g. "reset 10% of requests").
  The bare string form is unchanged and equivalent to `probability: 1.0`; each form round-trips
  unchanged through `GET /imposters`. `rift-lint` validates the object form (`probability` required
  and in `[0, 1]`, unknown fault types warned).

### Fixed

- **`--datadir` no longer silently drops malformed imposter files.** A file that can't be loaded
  (unreadable, invalid JSON, unresolvable scripts, or a creation failure) is now collected and
  reported together in a single prominent startup error listing every skipped file and why, instead
  of a per-file warning that was easy to miss — so a typo'd fixture that vanishes from the running
  set is visible. One bad file also no longer aborts loading of the remaining valid imposters.
- **Flow-level `ctx.state.ttl(seconds)` on the Redis backend** was a silent no-op — a script calling
  `ttl(3600)` got `true` back while nothing changed. It now re-stamps every key in the flow via
  `SCAN` + `EXPIRE`, matching the in-memory backend's behavior.
- **Non-positive `flowState.ttlSeconds`** (`< 1`) is now rejected with `400 Bad Request` at imposter
  creation instead of misbehaving later (in-memory: instant expiry of every write; Redis: a `SETEX`
  error on the first write).

## [0.13.1] - 2026-07-11

### Changed

- Renamed the `rift-core` crate to **`rift-mock-core`** to resolve a crates.io name collision — the
  `rift-core` name is owned by an unrelated project, which blocked publishing and had also frozen
  `rift-http-proxy` on crates.io at 0.4.0. This is purely a packaging change: the binary, FFI
  cdylib, Docker image, and npm package are functionally identical to 0.13.0. Rust code depending on
  the engine library from crates.io should switch the dependency name to `rift-mock-core`; the
  FFI / Docker / binary / npm distribution (what the language SDKs consume) is unaffected.

## [0.13.0] - 2026-07-10

This release lands the engine-side surface the official language SDKs build on — server-side
verification, the C-ABI admin long tail, a published conformance corpus, and runtime intercept —
plus a broad round of hot-path performance work and scripting/proxy fixes.

### Added
- **Server-side verification endpoint** (#494): `POST /imposters/{port}/verify` counts (and
  optionally returns) recorded requests matching a predicate set, evaluated by the engine's own
  predicate engine, and can return the closest non-match with per-clause `failedPredicates` for a
  readable diff. `flowId` scopes the count like `savedRequests`. A matching FFI symbol `rift_verify`
  gives embedded parity. This lets every SDK's `verify(match, times(n))` defer to the one true
  evaluator instead of re-implementing predicate matching (or shipping the whole journal over the
  wire), including operators impractical client-side (`xpath`, `inject`).
- **C-ABI admin long-tail symbols** (#491): twelve additive v2 FFI symbols — `rift_list_imposters`,
  `rift_get_imposter`, `rift_add_stub`, `rift_get_stub`, `rift_update_stub`, `rift_delete_stub`,
  `rift_clear_recorded`, `rift_clear_proxy_recordings`, `rift_set_imposter_enabled`,
  `rift_scenarios`, `rift_set_scenario_state`, `rift_reset_scenarios` — so an embedded SDK can drive
  the full admin surface without lazily booting an in-process admin plane over loopback HTTP. Each
  delegates to the same manager/imposter method its admin route uses.
- **`allowInjection` option for `rift_serve_admin`** (#492): the embedded admin plane now gates
  script/inject imposters submitted through it behind an explicit `allowInjection` flag (default
  off), mirroring the `--allowInjection` CLI gate.
- **Runtime intercept lifecycle over the admin API** (#493): `POST /intercept` starts the TLS-MITM
  intercept listener at runtime (201 with `{interceptPort, interceptUrl}`, 409 if already running),
  `GET /intercept` reports it (404 when not running), and `DELETE /intercept` stops it (204,
  idempotent). The endpoints are available on every server — a server started **without**
  `--intercept-port` can now enable intercept at runtime (SDK connect/spawn transport parity). The
  CLI flag, the FFI, and these routes all drive one shared listener, so a listener started by any
  surface is visible to the others. New FFI symbol `rift_stop_intercept` mirrors `DELETE /intercept`,
  and `rift_serve_admin` now serves the full `/intercept*` surface against the handle's listener
  (previously it 404'd). All endpoints are gated by `--apikey` like other admin routes.
- **SDK conformance corpus release artifact** (#460, #516, #517): each release now publishes
  `sdk-conformance-<version>.tar.gz` — imposter fixtures with `_verify` transcripts, `data/`, and
  injection modules, plus a README replay contract and a `manifest.json` index — so every SDK's CI
  replays the same fixtures for the engine version it pins and catches DSL/engine drift. An
  engine-side gate replays the whole corpus on every commit, and the SDK-relevant fixtures carry
  self-describing `_verify` sequences so SDKs assert behavior, not just DSL-expressibility.

### Changed
- **Case-insensitive `contains`/`startsWith`/`endsWith` predicates now fold ASCII only** (#480), for
  consistency with `equals` (already ASCII case-insensitive) and to avoid a per-request allocation on
  the matching hot path. Predicates over non-ASCII text that previously matched via Unicode case
  folding (e.g. `É` vs `é`) now require exact non-ASCII bytes; pure-ASCII matching is unchanged.

### Performance
- **Fewer per-request allocations in the imposter matching hot path** (#480, #508): the query string
  is parsed once per request (not once per predicate), case-insensitive string compares no longer
  allocate a lowercased copy of each side, and the request-context header map is captured directly
  instead of being rebuilt and re-validated per request.
- **Scripting hooks run inline on the async worker** (#501): Mountebank `inject`/`decorate`/predicate
  hooks no longer hop to `spawn_blocking` with a per-call timeout; each runs inline with a fresh Boa
  context, removing the blocking-pool round-trip on the scripted hot path.
- **Scenario-FSM Redis I/O offloaded off the tokio worker** (#503): the scenario state machine's
  Redis reads/writes run on `spawn_blocking` and fold `INCRBY`/`EXPIRE` into one round-trip, so a
  slow Redis backend no longer stalls a request worker.
- **HTTP connection pooling re-enabled for proxying** (#496): the proxy client reuses connections
  again (no fresh TCP+TLS handshake per proxied request) and drops a wasted clone of the recorded
  response body.
- **`InMemoryFlowStore` reaps expired entries** (#495): the in-memory flow store now evicts expired
  keys instead of growing unbounded, and `set_ttl` avoids a full-store scan.
- **Request-path regexes routed through the shared cache** (#489): path predicates and route anchors
  reuse cached/`LazyLock` regexes instead of recompiling per request.
- **Per-stub response work precomputed once** (#488): `_behaviors` and non-string bodies are parsed
  and precomputed once per stub rather than re-derived on every matching request.
- **`GET /imposters/{port}/savedRequests?match=` filters before cloning** (#485, #506): a `match=`
  query filters recorded requests over references before cloning, so it no longer deep-clones the
  whole journal to discard most of it.

### Fixed
- **A stub that only names a scenario (`scenarioName`) now gets a real flow store** (#514). Previously
  a stub declaring `scenarioName` without `requiredScenarioState`/`newScenarioState` landed on the
  no-op flow store, so `PUT /imposters/{port}/scenarios/{name}/state`, `POST .../scenarios/reset`
  (and the `rift_set_scenario_state`/`rift_reset_scenarios` FFI symbols) silently discarded the write
  and a subsequent read still showed the initial state. Any stub referencing a scenario now
  provisions an in-memory flow store, so the scenario surface behaves as documented.
- **Proxy `predicateGenerators.inject` failures no longer record a match-all stub** (#498, #507). When
  an inject predicate generator fails (script error, invalid output, script-pool failure, or
  timeout), Rift now skips auto-stub creation instead of silently recording a stub with empty
  predicates that would shadow every future request. The proxied response is still returned and
  carries an `x-rift-generator-error` header naming the failure. A generator that legitimately
  returns `[]` is unchanged.
- **Script timeouts are distinguished from broken-script errors in the response** (#499, #515): an
  `inject`/`decorate` script that times out now maps to `504` while a genuine script error stays
  `400`/`500`, so a slow script and a broken script are no longer conflated in the served response.
- **Per-imposter script state updates are atomic under concurrency** (#477, #486): concurrent
  requests mutating the same imposter's script/flow state no longer lose updates to a read-modify-
  write race.
- **`shellTransform` runs on the blocking pool, not a tokio worker** (#478, #505): the
  `shellTransform` behavior spawns its host subprocess on `spawn_blocking`, so a slow shell command
  can't stall a request worker.
- **Function-form `wait` behavior is clamped/capped like the Boa path** (#490, #504): the regex
  fallback for a JS-function `wait` now clamps and caps the delay the same way the Boa-evaluated path
  does.
- **Every `extern "C"` FFI entry point guards against panic-unwind across the C ABI** (#484, #487): a
  panic inside any FFI symbol is caught and converted to the symbol's error sentinel + `last_error`
  rather than unwinding across the C boundary (undefined behavior).
- **Abnormal intercept accept-loop exits are logged, not swallowed** (#523): a panicked intercept
  accept loop now surfaces its `JoinError` in a warning instead of letting `shutdown`/`stop` report
  success over a crashed listener.

## [0.12.0] - 2026-07-09

This release lands the scripting developer-experience redesign (Script API v2, file-based
authoring, declarative templating, and `rift script` tooling) and **removes the Lua engine** —
consolidating on Rhai and JavaScript. Removing Lua also eliminates the system-LuaJIT dynamic-link
dependency, so the published `librift_ffi` cdylibs are now self-contained on every platform.

### Added
- **Script API v2 — a unified `ctx` object** (#442). Injects and decorates receive a single `ctx`
  with `http(...)`, `delay(...)`, `reset()`, and `pass()` result constructors, replacing the older
  positional calling conventions.
- **Flow-scoped script state** (#443): `ctx.state` with `get_or` / `incr_by` / `cas` / `ttl`,
  backed by an auto-provisioned in-memory store; a configured backend that is down fails loud
  rather than silently dropping state.
- **Declarative response templating** (#444): `{{ … }}` template functions in response bodies and
  headers, behind an opt-in `templated` flag.
- **Script tooling** (#445): `rift script check` and `rift script run` subcommands, plus a
  debug-mode script trace showing which hook ran and its decision.
- **File-based script authoring** (#441): `file:` / `ref:` script references and a named script
  library resolved from `--scripts-dir` (`RIFT_SCRIPTS_DIR`).
- **Mountebank v2 fidelity bundle** (#438): the v2 `config`-first inject convention, a native
  `logger`, script timeouts, `--allowInjection`, error parity, wait functions, and EJS stringify.
- **`request.pathParams`** (#435): route-pattern path-parameter extraction as a stub field, usable
  in predicates and response templating.
- **`ffi-manifest.json` release asset** (#459): a `{version, abi, artifacts[]}` map of the
  `librift_ffi` cdylibs (platform / file / sha256 / url) so SDK consumers resolve natives without
  hardcoding release-URL patterns.
- **x86_64 musl `librift_ffi` cdylib** (#463): a dynamically-linked musl `.so`, for embedded SDK
  tests on Alpine-based CI hosts.
- **`rift_stub_warnings` FFI accessor** (#423): stub-overlap analysis warnings over the C-ABI, so
  embedded consumers get the config-lint the HTTP admin plane already exposed.

### Changed
- **Stub-overlap analysis moved into the engine, cached, and bounded to O(n)** (#423). It is
  computed once on stub mutation and cached — no per-`GET` recompute — exact-duplicate detection is
  O(n) via predicate hashing, and warnings are capped with a `truncated` summary. A multi-tenant
  imposter with hundreds of overlapping stubs no longer stalls admin create/read (seconds and
  hundreds of MB → milliseconds and a few MB), and embedded and standalone share one code path.

### Fixed
- **Predicate-inject errors fail loud** (#440) with a Mountebank-shaped `400`, instead of silently
  falling through.
- **Per-imposter script state is keyed on the bound port** (#439), so auto-bind imposters cannot
  collide in the process-global inject-state map.
- **Released `librift_ffi` cdylibs are self-contained** (#469): a release-time gate asserts each
  cdylib's dynamic imports are stock system libraries only, so they `dlopen` on stock hosts without
  extra packages.

### Removed
- **The Lua scripting engine** (#451) — Rift consolidates on Rhai and JavaScript. This also removes
  the system-LuaJIT dynamic-link dependency that made prior `librift_ffi` cdylibs fail to load on
  hosts without LuaJIT installed. **Breaking:** migrate Lua scripts to Rhai or JavaScript.
- **The v1 `should_inject` scripting contract** (#454). **Breaking:** use the v2 `ctx` API.
- **The dead `RIFT_STRICT_FLOW_STORE` toggle** (#456).

## [0.11.3] - 2026-07-08

### Added
- **`rift_start_intercept` accepts a caller-provided intercept CA** (#429). New optional
  `caCertPath`/`caKeyPath` options load a committed CA (via `CertificateAuthority::load_pem`)
  instead of only minting a fresh ephemeral one, so independent embedded instances can share a
  committed trust anchor — a long-lived containerized SUT can trust a CA that pre-exists its JVM
  startup. A half-configured pair (only one of the two) is rejected with a clear both-or-neither
  error; omitting both generates a CA exactly as before (unchanged default). The load-or-generate
  logic is now shared with the container adapter's `--intercept-ca-cert`/`--intercept-ca-key`.

## [0.11.2] - 2026-07-08

### Fixed
- **`rift_start_intercept` now returns a truthful `interceptUrl`** (#425). It previously hardcoded
  `http://127.0.0.1:<port>` even when the listener bound a non-loopback host, misreporting the
  endpoint as loopback for a `0.0.0.0`/NIC-IP bind (e.g. for cross-container reachability). The URL
  and port are now derived from the listener's real bound address; the loopback default is unchanged.

### Changed
- **Refreshed the Rift vs Mountebank benchmark and README performance numbers** (#419). Added a
  no-Docker, direct-process harness (`tests/benchmark/scripts/bench_direct.py`) that runs each engine
  in isolation on disjoint ports and asserts every scenario's response body actually matches before
  measuring, replacing the stale figures with reproducible ones.

## [0.11.1] - 2026-07-07

### Added
- **`rift_flow_state_get` now gives an unambiguous "not found" signal** over the embedded C-ABI
  (#416). It previously returned `null` both for an absent key and for an error, so a host could
  not tell the two apart; the call now reports "not found" distinctly from a genuine failure.

### Fixed
- **PKCS#12 truststore export is now loadable as a JVM trust store** (#417). `export_pkcs12` (and
  thus `rift_intercept_export_truststore` / `GET /intercept/truststore.p12`) previously wrote the CA
  as a bare cert bag that a JVM `TrustManagerFactory` did not surface as a trust anchor — TLS
  validation failed with "the trustAnchors parameter must be non-empty". The export now carries the
  trusted-certificate marker `keytool` writes, so the CA loads as a trust anchor, matching the JKS
  export.

## [0.11.0] - 2026-07-07

### Added
- **Direct C-ABI over `librift_ffi` for the embedded control plane**, so a host process (e.g. a JVM
  test harness) can drive Rift without standing up the loopback admin-HTTP server. `rift_serve_admin`
  becomes optional.
  - Scenario state & correlated spaces (#412): `rift_flow_state_get` / `rift_flow_state_put` /
    `rift_flow_state_delete` and `rift_space_add_stub` / `rift_space_list_stubs` /
    `rift_space_delete` / `rift_space_recorded` wrap the same `ImposterManager`/`Imposter` methods
    the admin-HTTP handlers use.
  - Intercept listener & rules (#413): `rift_start_intercept`, `rift_intercept_add_rules`,
    `rift_intercept_list_rules`, `rift_intercept_clear_rules`, `rift_intercept_ca_pem`,
    `rift_intercept_export_truststore` (writes a JVM-consumable trustStore to a file path), and
    `rift_stop` for teardown — the FFI equivalent of the intercept admin API shipped in 0.10.0.
  - Regenerated cbindgen header; documented in `docs/embedding/ffi.md`.

## [0.10.0] - 2026-07-07

### Added
- **Built-in intercept/redirect proxy mode (TLS-MITM).** Rift can now sit in the request path as an opt-in HTTPS forward proxy to mock an external dependency whose host the system-under-test hard-codes (e.g. a feature-flag SDK that always fetches `https://cdn.example.com/config.json`) — replacing an external mitmproxy sidecar with no committed crypto. It accepts HTTP `CONNECT`, TLS-terminates using an intercept CA (generated at startup, or loaded from PEM) that mints a per-SNI leaf certificate on demand, matches the decrypted request with the existing predicate engine, and either serves an inline stub or forwards it to an imposter port.
  - Admin API: `POST`/`GET`/`DELETE /intercept/rules`, plus `GET /intercept/ca.pem` and `/intercept/truststore.p12` / `/intercept/truststore.jks` to export the CA cert and a ready-to-use PKCS#12 or JKS truststore (with password) for a SUT to trust.
  - Standalone binary: `--intercept-port` (env `RIFT_INTERCEPT_PORT`) starts the listener; `--intercept-ca-cert` / `--intercept-ca-key` load an existing CA, otherwise one is generated in-memory. The rule store and CA are shared with the admin API.
  - Documented in `docs/features/intercept-proxy.md` with a runnable end-to-end example. Non-goals: HTTP/2, WebSocket proxying, chunked request bodies, and transparent (non-`CONNECT`) interception.

## [0.9.1] - 2026-07-04

### Fixed
- Release artifacts now include the `rift-verify` binary — the platform tarballs, the Windows zip, and the Homebrew formula ship it alongside `rift`, `rift-lint`, and `rift-tui`. It was built by the release job but never packaged, so no prior release contained it.

## [0.9.0] - 2026-07-04

### Added
- HTTP/2 and h2c support via hyper auto-negotiation on HTTP and HTTPS listeners; `RIFT_DISABLE_HTTP2` escape hatch forces HTTP/1-only.
- Socket tuning: `TCP_NODELAY` is on by default, with `RIFT_TCP_NODELAY` and `RIFT_TCP_BACKLOG` knobs.
- Feature-gated `mimalloc` global allocator (default-on for `rift-http-proxy`).
- `rift-verify -o json` machine-readable summary; ANSI/banner suppression when stdout is piped or `NO_COLOR` is set.
- Opt-in strict behaviors: per-imposter `strictBehaviors` flag / `RIFT_STRICT_BEHAVIORS` env var returns `500` on a `decorate`/`shellTransform`/binary failure instead of the lenient fallback.
- Opt-in `RIFT_STRICT_FLOW_STORE` env var raises on script flow-store op failures in all three engines.
- `flow_store.last_error()` lets scripts distinguish a down backend from an empty result.
- Script wall-clock timeout (`_rift.scriptEngine.timeoutMs`, default 5000 ms) plus JS loop-iteration and recursion bounds.

### Changed
- `POST /admin/reload` is now incremental (diff-based): unchanged imposters and stubs keep their runtime state, and the response reports `created`/`replaced`/`stubPatched`/`deleted`.
- Top-level `fault` responses reset the connection at the transport level (Mountebank parity) instead of returning HTTP 502.
- `decorate`/`shellTransform`/binary behavior failures are signaled via `x-rift-<behavior>-error` response headers instead of being served silently.
- Bare `jsonpath` selectors (no leading `$`) are treated as root-relative.
- Flat stub responses (top-level `statusCode`/`headers`/`body`, no `is` wrapper) are accepted and served identically to the wrapped form.

### Fixed
- An unknown `flowState` backend, or an unreachable/misconfigured redis flow-store, now fails imposter creation with `400` instead of silently downgrading to a no-op store.
- Runaway `_rift.script` execution is bounded so it can no longer wedge the engine.

## [0.8.0] - 2026-07-01

### Added
- Past offsets in date templates: `{{DAYS-N}}` / `{{MONTHS-N}}` (complementing `{{DAYS+N}}` / `{{MONTHS+N}}` / `{{NOW}}`).
- `rift-verify --verify-dynamic` asserts dynamic behaviors (inject/proxy/script, faults, binary mode, inline-flag regex, exists-query).

### Changed
- Flow-state config: `flowIdSource` is now flattened under `_rift.flowState`; the `mountebankStateMapping` wrapper was dropped.

### Fixed
- TCP faults now take precedence over error faults in `_rift.fault`.
- Multi-value response headers are preserved under `copy` / `lookup` / `decorate` behaviors.
- Request templates, `shellTransform`, and metrics are applied on the static-response path.
- `rift-lint` no longer emits W009 on Rhai or Mountebank config-arrow `decorate` scripts, and accepts multi-value header string arrays (E018).

## [0.7.0] - 2026-06-29

### Added
- Per-imposter HTTPS: imposters declared with `protocol: https` terminate TLS.
- Single-port imposter access via the `/__rift/:port/<path>` gateway.
- Date templates in response bodies: `{{DAYS+N}}`, `{{MONTHS+N}}`, `{{NOW}}`.

### Fixed
- `decorate` behavior supports the JavaScript `config =>` calling convention.

## [0.6.0] - 2026-06-29

### Added
- Real connection-level TCP faults for `_rift.fault.tcp`.
- Multi-value header support for responses and recorded requests.
- Hot-reload of imposters via `POST /admin/reload`.

## [0.5.0] - 2026-06-29

### Added
- Id-addressed stub operations: add / replace / delete a stub by its `Stub.id` (`/imposters/:port/stubs/by-id/:id`).
- `rift-ffi` crate: a C-ABI over `ImposterManager` for embedding Rift in-process.

### Changed
- Engine extracted into the CLI-free `rift-core` crate; shared types moved to `rift-types`.

## [0.4.0] - 2026-06-29

### Added
- Declarative stateful scenarios (`requiredScenarioState` / `newScenarioState`) plus flow-state admin inspection endpoints.
- First-class correlated isolation ("spaces"): space-scoped stubs and per-space teardown.
- `defaultForward` fallback proxy for unmatched requests.

## [0.3.0] - 2026-06-28

### Added
- Filter recorded requests by header or flow id (`GET /imposters/:port/savedRequests?match=...`).

### Fixed
- Script request headers are exposed with lowercase keys.
- `lookup` behavior applies on direct imposter responses.
- `rift-lint` accepts the imposters-wrapper and bare-array config shapes.

## [0.2.0] - 2026-06-28

### Added
- `--apikey` flag for admin API authentication.
- `--datadir` write-through persistence for live imposter mutations.
- `--rcfile` defaults merging and `save` defaults.
- EJS token preprocessing in `--configfile` (`<% include %>`, `<%= process.env.X %>`).
- `inject` predicate operation and `predicateGenerators` support in proxy recording.
- `?list=true` query parameter on `GET /imposters`.
- `--log` file logging (via tracing-appender); accepts `--noParse`, `--formatter`, `--protofile` for Mountebank compatibility.

### Fixed
- `allowCORS` injects CORS headers and handles `OPTIONS` preflight.
- Keep-alive connections are closed on imposter delete.
- XPath namespace maps (`ns`) are applied during predicate evaluation.
- Proxy pipeline preserves multi-valued headers and binary response bodies; recording race conditions and memory-exhaustion risks fixed.
- Numerous predicate-matching correctness fixes (`exists`, `except`, `deepEquals`, multi-valued query params, regex flags).

## [0.1.0-RC1 .. 0.1.0-RC13] - 2025-11-28 .. 2026-01-04

Initial release-candidate series establishing the Mountebank-compatible core: imposters, stubs,
predicates, responses, behaviors, proxy/record, and the `_rift` extension namespace (fault
injection, multi-engine scripting, flow state).

[Unreleased]: https://github.com/achird-labs/rift/compare/v0.14.0...HEAD
[0.14.0]: https://github.com/achird-labs/rift/compare/v0.13.6...v0.14.0
[0.13.6]: https://github.com/achird-labs/rift/compare/v0.13.5...v0.13.6
[0.13.5]: https://github.com/achird-labs/rift/compare/v0.13.4...v0.13.5
[0.13.4]: https://github.com/achird-labs/rift/compare/v0.13.3...v0.13.4
[0.13.3]: https://github.com/achird-labs/rift/compare/v0.13.2...v0.13.3
[0.13.2]: https://github.com/achird-labs/rift/compare/v0.13.1...v0.13.2
[0.13.1]: https://github.com/achird-labs/rift/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/achird-labs/rift/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/achird-labs/rift/compare/v0.11.3...v0.12.0
[0.11.3]: https://github.com/achird-labs/rift/compare/v0.11.2...v0.11.3
[0.11.2]: https://github.com/achird-labs/rift/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/achird-labs/rift/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/achird-labs/rift/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/achird-labs/rift/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/achird-labs/rift/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/achird-labs/rift/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/achird-labs/rift/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/achird-labs/rift/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/achird-labs/rift/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/achird-labs/rift/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/achird-labs/rift/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/achird-labs/rift/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/achird-labs/rift/compare/v0.1.0-RC13...v0.2.0
[0.1.0-RC13]: https://github.com/achird-labs/rift/releases/tag/v0.1.0-RC13
