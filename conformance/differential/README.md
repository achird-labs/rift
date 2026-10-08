# Differential conformance: Mountebank is the oracle

Every case here runs on a real Mountebank and on the Rift binary under test, and the only assertion
is that the two answered the same. Nobody writes an expected value: the other engine is the
expectation. This replaces `tests/compatibility/` (issue #1341), which compared status and parsed
body only, never headers and never the stored imposter, and so could not see #1329, #1333 or #1334.

## Running it

```bash
npm install --prefix ~/bench-mb mountebank@2.9.1   # once; the oracle
cargo build -p rift-http-proxy                     # the server binary the harness spawns
cargo test -p rift-differential -- --nocapture
```

`cargo test --workspace` runs it too (it builds the server first). The `differential` job in
`.github/workflows/ci.yml` runs it on every pull request.

| Variable | Effect |
|:---------|:-------|
| `RIFT_MB_BIN` | Mountebank's `mb`. Default: `~/bench-mb/node_modules/.bin/mb`, then `mb` on `PATH` |
| `RIFT_SERVER_BIN` | The Rift server to test. Default: `rift-http-proxy` in the target directory the test was built into, refused when it is older than the engine sources (`cargo test -p rift-differential` does not rebuild it) |
| `RIFT_DIFFERENTIAL_REQUIRE_MB` | Fail, instead of skipping, when no Mountebank is found (CI sets it) |
| `RIFT_DIFFERENTIAL_FILTER` | Run only the cases whose name contains this text (the stale-entry check is then off) |
| `RIFT_DIFFERENTIAL_DUMP` | Write every difference found, explained or not, to this file as JSON — the raw material for a precise allow-list entry |

Mountebank is pinned to 2.9.1, the version the published benchmark uses (`benchmark-publish.yml`).
2.9.2 exists only as a Docker image, not on npm.

## What is compared

Both engines start once (`--allowInjection --localOnly`, working directory `sdk-conformance/corpus`
so fixture paths like `data/products.csv` resolve). For each case:

1. Every step is sent to both engines. Cases are written with *logical* ports (`4545`); each engine
   gets its own free ports, and every port in a request is mapped forward and every port in an
   answer mapped back, so ports never differ.
2. Every answer is compared: status, every header except `Date`, `Connection`, `Keep-Alive`,
   `Transfer-Encoding`, `Content-Length` and `Server` (names case-insensitive), and the body —
   byte for byte, located by JSON pointer when both bodies are JSON. Admin-API JSON is compared as
   parsed values after canonicalisation (below). A request that gets no answer compares only the
   failure class (connect, timeout, transport).
3. After the last step, every imposter either engine holds is fetched as `GET /imposters/:port` and
   with `?replayable=true`, canonicalised, and diffed.
4. Both engines `DELETE /imposters`.

Canonicalisation (`src/canon.rs`) removes only what is not a contract, at the structural level it
lives at: `_links`; the Rift-only `stubCount`, `enabled` and `_rift`; `_proxyResponseTime`; in a
stub a proxy recorded (Rift's `recordedFrom`, Mountebank's `_proxyResponseTime`), the ignored headers
above and a measured `addWaitBehavior` wait; and everything in a recorded request except `method`,
`path`, `query`, `body`, `headers` and `form` (names case-folded, transport headers dropped). A stub
the case wrote keeps every header it was given, and nothing inside a response body or a predicate is
touched.

Locations name what was compared: `step[3]` is an imposter request, `step[3].admin` an admin call,
`step[3].admin.export` / `.admin.import` a re-import, `step[3].rep[1]` a repeated send,
`stored[4545]` / `replayable[4545]` the final imposters.

## Known differences: `allowlist.json`

A difference fails its case unless an entry in `allowlist.json` names it. An entry is either

- `documented` — a deliberate deviation, with `doc` and a verbatim `quote`. The
  `allow_list_is_valid_and_its_citations_resolve` test fails when the quote is no longer in the doc,
  so deleting the doc line turns the harness red; or
- `known-bug` — a divergence that is not intended, with its `reason` (and `issue` once filed).

`case` names one case, or `*`; `location` is a glob over the difference location (`step[2].body`,
`stored[4545]/stubs/0/responses`) and also covers what lies below it. `mountebank` / `rift` pin a
side's value exactly, `mountebankMatches` / `riftMatches` as an anchored regex, and `relation:
"rift-quotes-mountebank"` ties the two (`200` against `"200"`). A `*` case must pin both values, so
it is a precise claim about what Rift does differently everywhere, never a blanket ignore of a header
or a field. The one other wildcard is the `json-text` class: a served JSON body with the same value,
which Rift serves compact and key-sorted. A difference is credited to the first entry that matches,
and an entry that matches nothing in a full run fails the run, so a fixed divergence cannot leave its
excuse behind.

Case-specific entries pin one leaf each, with both values, so a change in Rift's answer at that spot
is red. Run with `RIFT_DIFFERENTIAL_DUMP` to get the exact locations and values for a new one. Values
that legitimately vary between runs or platforms (an OS-assigned port in a message, `errno`, a
JavaScript stack) are pinned by a regex over their stable part.

The deliberate deviations are listed for users in
[`docs/mountebank/differences.md`](../../docs/mountebank/differences.md).

## Cases

`cases/*.json`, one file per source:

- `admin_api.json` … `stub_management.json` — the request and imposter shapes of the 11 retired
  `tests/compatibility/features/*.feature` files, one case per scenario. Assertions were dropped:
  the oracle replaces them.
- `sdk-corpus.json` — the Mountebank-drivable fixtures of `sdk-conformance/corpus/imposters/`,
  loaded from the corpus itself, `_verify` stripped, each `_verify.sequence[].request` sent in order.
- `targeted.json` — the miss classes the retired suite could not see (#1329, #1333, #1334,
  `deepEquals` on query), and one case per documented deviation the allow-list cites.

A step is `{"do": "admin", "method", "path", "body"?}`, `{"do": "send", "port", "method"?, "path"?,
"headers"?: [[name, value]], "body"? | "bodyOfSize"?, "times"?}`, `{"do": "concurrent", ...,
"times"}` (the answers are compared as a multiset), or `{"do": "reimport"}` (each engine re-imports
its own `?replayable=true` export). A string `body` is sent verbatim, any other JSON value serialised.

### SDK-corpus fixtures not driven

| Fixture | Why |
|:--------|:----|
| 04, 17 | `_rift.fault` responses |
| 06 | `_rift` flow state and Rhai scripts |
| 10 | `space` (Rift-only) |
| 11 | `{{...}}` response templating (Rift-only) |
| 20 | `_rift` and Rift-only aliases |

Single stubs dropped from driven fixtures are listed with their reason in `cases/sdk-corpus.json`
(`dropStubs`): Mountebank refuses the whole imposter for each of them.

### Retired scenarios not ported here

The retired suite's `@rift-only` and `@skip` scenarios ran against Rift alone; there is no oracle for
them. Each is either covered elsewhere or ported to
`crates/rift-http-proxy/tests/compat_rift_only.rs`:

| Retired scenario(s) | Now |
|:--------------------|:----|
| `debug_mode.feature` (all 9) | `compat_rift_only.rs` `debug_mode_*` |
| `proxy.feature` pathRewrite (3) | `compat_rift_only.rs` `path_rewrite_*` |
| `admin_api.feature` get all stubs / stub by index, and the three stub-read 404 scenarios | `compat_rift_only.rs` `get_all_stubs_for_an_imposter`, `get_stub_by_index`, `stub_reads_answer_404_for_a_missing_imposter_or_index` (Mountebank has no `GET .../stubs` route) |
| `rift_extensions.feature` script validation (7) | `compat_rift_only.rs`; the Lua and missing-`should_inject` scenarios assert today's behaviour (#450, #453) |
| `rift_extensions.feature` error fault with headers, empty `_rift`, per-client header lookup | `compat_rift_only.rs` |
| `stub_management.feature` shadowed / duplicate-id warnings | `compat_rift_only.rs` `warnings_for_*` |
| `stub_management.feature` stub id accepted | `crates/rift-http-proxy/tests/admin_api_integration.rs` (stub by id) |
| `mountebank_compatibility_gaps.feature` keyCaseSensitive, `host` field | Mountebank runs them: `targeted.json` |
| `rift_extensions.feature` flow state, latency and error faults, `_behaviors` + fault, predicates / cycling / default response with `_rift`, plain config | `crates/rift-http-proxy/tests/rift_extensions.rs` |
| `rift_extensions.feature` circuit breaker, token bucket, retry, session affinity, cascading failure, idempotency, A/B, feature flag, saga (2), health aggregation, coalescing, failover | Not ported: each is JavaScript logic over the inject + flow-state mechanism `rift_extensions.rs` and corpus fixtures 05/06 already cover; none exercises engine behaviour of its own |
| `alternative_formats.feature` `rules` alias, predicates-over-rules, `delayRange`, `recordedFrom` | `crates/rift-mock-core/src/imposter/types.rs` unit tests and corpus fixture 20 |
| `responses.feature` shellTransform (2, one `@skip`) | `behaviors/transform.rs` unit tests, `issue_1198_behavior_program.rs`; the served difference is pinned by `targeted.json` |
