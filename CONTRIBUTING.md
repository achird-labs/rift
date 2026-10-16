# Contributing to Rift

Thanks for looking. Rift is **beta** and maintained by one person, so the most useful thing you can
do is tell me where it is wrong — a reproduction, a misleading doc, or a benchmark that does not
replicate is worth more to me than a feature.

Everything here is Apache-2.0. There is no paid edition and no contributor licence agreement.

## The most valuable issue you can file

**A benchmark of mine that you think is unfair.** The whole harness is in
[`tests/benchmark`](tests/benchmark/) and it runs the other engines too, not just this one. If a
competitor is misconfigured, if a scenario flatters Rift, or if a number does not reproduce on your
hardware — say so. It gets re-run and published as a correction, including when the correction
narrows a gap I have claimed. That has happened before and it will happen again.

When reporting one, include the hardware, the load generator and its concurrency level. A ratio
without its conditions attached cannot be acted on.

## Where things live

| | |
|---|---|
| `crates/rift-types` | shared wire types (imposters, stubs, predicates) |
| `crates/rift-mock-core` | the engine library: imposters, matching, behaviors, scripting, flow state, listeners and TLS, the SPI traits |
| `crates/rift-http-proxy` | the `rift` server binary and `rift-verify`: CLI, admin API, front door, intercept proxy |
| `crates/rift-ejs` | the EJS subset the config loader and `rift-lint` both render |
| `crates/rift-lint` | the `rift-lint` library and CLI |
| `crates/rift-tui` | the `rift-tui` terminal UI |
| `crates/rift-ffi` | the C ABI every SDK binds to |
| `sdk-conformance/` | the shared corpus each SDK must pass |
| `tests/benchmark/` | the published benchmark harness |

The four SDKs live in their own repos — [rift-java](https://github.com/achird-labs/rift-java),
[rift-node](https://github.com/achird-labs/rift-node),
[rift-go](https://github.com/achird-labs/rift-go),
[rift-scala](https://github.com/achird-labs/rift-scala) — and all bind the same `rift-ffi` surface.
**A change to the C ABI is a change to four downstream repos**, so it needs an issue first.

Clustering lives in [rift-cluster](https://github.com/achird-labs/rift-cluster), which vendors this
repo read-only. Generic capability belongs here, where every user gets it.

## Getting set up

```sh
git clone https://github.com/achird-labs/rift.git
cd rift
cargo test --workspace
```

## Before you open a PR

CI runs these, so run them first:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Clippy is `-D warnings`. There is no warning budget. Doctests are part of the suite — the examples
in the docs are compiled, which is why they are trustworthy, and why breaking one fails the build.

If you touched the FFI surface, also run `scripts/verify-ffi-cdylib.sh`. If you touched
Mountebank-visible behaviour, replay the conformance corpus:

```sh
cargo test -p rift-http-proxy --test corpus_replay
```

That corpus in [`sdk-conformance/`](sdk-conformance/) is the single source of truth for DSL-to-engine
parity, and all four SDKs replay it in their own CI — so a fixture that breaks here breaks four
downstream repos. Compatibility is the promise this project is built on, and it is tested rather
than asserted.

If you wrote or edited a config example — a corpus fixture, a file under `docs/demo/` or
`examples/`, or a ```` ```json ```` block in `docs/` — check that the engine reads every key in it:

```sh
cargo test -p rift-http-proxy --test issue_1343_unread_keys
```

The engine accepts a key it does not read and drops it without a word (a misspelled
`casesensitive`, a block at the wrong level), so this test is where that is caught. It also lints
each docs example. A block that shows rejected input on purpose, or JSON that is not a config (an
API response body), goes unchecked with a `<!-- rift-lint: skip -->` line directly above its fence.
A key that is unread by choice goes in the test's `ALLOWED` list, with the reason.

## Mutation testing

A scheduled workflow, [`mutants.yml`](.github/workflows/mutants.yml), runs `cargo-mutants` weekly over
predicate matching, behaviors, proxy generation and `rift-types`' predicate model. It is not a PR
check (each of its ~650 mutants is a rebuild and a test run, about 17 hours of compute, split
across 12 parallel shards of about 1.5 hours) and never blocks a merge: it reports surviving
mutants, meaning code a test claims to cover but cannot tell was changed. The `Merge shard results`
job writes the merged counts, survivors and timeouts to the run summary, and each shard's
`mutants.out/` is attached as a `mutants-out-<k>` artifact. Trigger it by hand from the Actions tab (`workflow_dispatch`), or locally with
`cargo install cargo-mutants --locked` and the same `-p`/`-f` arguments the workflow uses; add
`--list` to see the mutants without running them, or narrow `-f` to one file to iterate quickly.
[`.cargo/mutants.toml`](.cargo/mutants.toml) holds the exclusions.

A surviving mutant (a line of `mutants.out/missed.txt`) is one of three things:

- **A test gap.** Write the test that fails on that mutant. The kill must be a test in the mutated
  crate (`cargo mutants -p <crate>` runs only that package's tests), so a case in
  `conformance/differential/cases/` cannot kill it: it runs in `rift-differential` against a prebuilt
  binary. Add a differential case on top as the parity pin when Mountebank's answer is the point.
  Group a cluster of survivors in one function into one test, not one per mutant.
- **An equivalent mutant**, which changes no observable behaviour (`>=` against `>` where equality
  cannot happen). Delete the redundant code when it is simply dead (for example a match arm the
  fallback already covers); otherwise add an `exclude_re` entry to `.cargo/mutants.toml` with a
  one-line reason (`#[mutants::skip]` needs the `mutants` crate, which is not a dependency).
- **Unobservable by design.** Add an `exclude_re` entry to `.cargo/mutants.toml` with a reason.

A survivor that replaces a value with `Default::default()` on a data path deserves a look first: it
means nothing tests the case where that value is wrong, which is where silent fallbacks hide.

## Model-based tests

[`crates/rift-mock-core/tests/model/`](crates/rift-mock-core/tests/model/) drives random sequences of
admin operations and requests (stub add/delete/move/replace, scenario and flow-state writes, hot
reload, re-creating the imposter, and requests through `proxyOnce`/`proxyAlways` stubs to a real
upstream) against a live `ImposterManager` and a small reference model, and compares the two after
every step: each response, the stub list with its recordings, every scenario and flow-state value,
the upstream's request count, and that a changed answer never reuses an older `Last-Modified`. It
runs in `cargo test` at 64 cases; set `PROPTEST_CASES` for more, as the nightly
[`model-tests.yml`](.github/workflows/model-tests.yml) does (2048).

A failure prints the shrunk operation sequence and writes its seed to
`crates/rift-mock-core/tests/proptest-regressions/model.txt` (the nightly run uploads it as an
artifact). Check the seed in with the fix, and add the sequence as a named test in `main.rs` too: a
seed regenerates its case through the strategies, so it stops meaning the same case once they
change. The model follows Mountebank, except where `model.rs` names a known divergence by issue;
fixing one of those means deleting that branch of the model.

## Fixed test ports

Tests do not write fixed ports. `crates/rift-mock-core/tests/test_port_uniqueness.rs` refuses any
integer literal in **15000-24999** under `crates/*/tests/**` and `crates/*/src/**`, and names each
`file:line` it finds.

A fixed port is bound twice whenever the same test binary runs twice at once: two worktrees
building side by side, `--lib` and `--tests` in two terminals, or two loops of one file. The
collision is silent. Imposter listeners bind with `SO_REUSEADDR` and `SO_REUSEPORT`, so neither
binder gets `AddrInUse` — the kernel load-balances, and the loser receives the *other* imposter's
responses, failing on a decode or status mismatch that points nowhere near the cause. **CI being
green is not evidence a port is free.** Take a port from, best first:

1. **The manager.** Omit `port` (or pass `0`); `create_imposter` returns the port it bound, and it
   keeps the probe listener bound, so a concurrent run simply gets another one. Through the admin
   API, read `port` back from the `201` body. This covers almost every in-process test.
2. **The OS, for a listener the test owns.** Bind `127.0.0.1:0` and use `local_addr()`, as the
   admin, front-door and intercept listeners do; an upstream server for a proxy test is the usual
   case.
3. **`free_port()`** (bind `:0`, read the port, drop the listener) only when the port has to be
   written into text before anything binds it: a `--configfile` document, a CLI argument, a URL in
   a stub, or a test about explicit ports. Copy the six-line helper as the files already using it
   do.

Never learn the next auto-assigned port and then pass it explicitly: that reopens the race the
manager closes. A number in the block that is not a port (a duration, a size) is written with a
separator, `20_000`, which the guard skips.

A test client that talks to imposters should not pool connections
(`reqwest::Client::builder().pool_max_idle_per_host(0)`): a deleted imposter's port is the next
auto-assigned one, and a pooled connection to it is dead. See
`crates/rift-mock-core/tests/model/main.rs:38`.

## The Plane A specification

[`conformance/features/`](conformance/features/) holds the scenarios Rift must satisfy, and the
runner in [`conformance/runner/`](conformance/runner/) (cucumber-rs) executes every one of them. If
you change behaviour a scenario asserts, change the scenario in the same pull request; a new feature
file also needs its entry in `conformance/manifest.json` and in the runner's
`spec::PORTED_FEATURES`. It needs only Rust to run locally, against a staged build:

```bash
RIFT_BUILD_DIR=/path/to/staged/build RIFT_MAX_BODY_SIZE=10485760 \
  cargo test -p rift-conformance-runner --test plane_a
```

Staging the build, the modes and the other variables are in
[`conformance/README.md`](conformance/README.md). A step the Rust runner does not know yet goes in
`conformance/runner/tests/plane_a.rs`, its text matching the feature's byte for byte.

## Mountebank is the oracle

[`conformance/differential/`](conformance/differential/) runs every case on a real Mountebank and on
your build, and fails on any difference in a response or a stored imposter. If you touched
Mountebank-visible behaviour, run it:

```sh
npm install --prefix ~/bench-mb @mbtest/mountebank@2.9.5   # once
cargo test -p rift-differential -- --nocapture
```

Without Mountebank installed the test skips; the `differential` CI job always runs it. A difference
it reports is either a bug to fix or a deliberate deviation: then document it (most belong in
[`docs/mountebank/differences.md`](docs/mountebank/differences.md)) and add an entry to
`conformance/differential/allowlist.json` that quotes the doc line. A case for new behaviour is a
few lines of JSON in `conformance/differential/cases/` — no expected values, the other engine is the
expectation.

## What a good PR looks like

- **One concern.** A PR that fixes a bug and reformats a module is two PRs.
- **A test that fails without the change.** For a bug fix, the test is the evidence the bug existed.
- **A description that says why, not what.** The diff already says what.
- **Mountebank compatibility is not negotiable.** If a change makes Rift diverge from Mountebank's
  documented behaviour, it needs an explicit argument for why, in the PR description.

Commit messages: imperative mood, and explain the reasoning where it is not obvious.

## Reporting a bug

Include the version (`rift --version`), the platform, and the smallest `imposters.json` that
reproduces it. If Mountebank behaves differently on the same config, say what it does instead —
that turns a bug report into a conformance case.

## Security

Do not open a public issue for a vulnerability. Use GitHub's private
[security advisory](https://github.com/achird-labs/rift/security/advisories/new) flow.
