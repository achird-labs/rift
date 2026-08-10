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
| `crates/rift-mock-core` | matching engine, imposters, predicates, proxy |
| `crates/rift-http-proxy` | the server binary |
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
