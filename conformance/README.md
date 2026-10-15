# Rift conformance

Two trees, with opposite questions:

| Tree | Question it answers | Who runs it |
|---|---|---|
| [`features/`](features/) + fixtures | **What Rift must do.** The Plane A specification: Gherkin scenarios driving a real `rift`, `rift-lint` and `rift-verify` binary. | The [rift-conformance](https://github.com/EtaCassiopeia/rift-conformance) runner, with `RIFT_FEATURES_DIR` pointed here |
| [`differential/`](differential/) | **What Mountebank does.** Every case is replayed on Mountebank 2.9.1 and on this build, and any difference not on its documented allow-list fails. | `cargo test -p rift-differential` (see its README) |

The specification lives in this repository so a behaviour change and the scenario that pins it land
in one pull request. The runner (Scala, zio-bdd) stays in rift-conformance and reads this tree; it
holds no copy of its own once this ships in a release.

## Layout (the contract a runner consumes)

```
conformance/
  README.md                 this file
  manifest.json             schemaVersion, engineVersion, and every feature file
  RUNNER_REF                the rift-conformance git ref the PR-time runner job checks out (#1340)
  features/rift/NN-*.feature  one file per Plane A suite; a suite loads classpath:features/rift/NN-*.feature
  rift-fixtures/**          imposter documents, configfiles, TLS/intercept PEMs the scenarios load
  rift-lint-fixtures/*.json rift-lint rule fixtures (one per rule exercised)
  rift-scripts/*            Rhai/JS scripts for the scripting scenarios
  differential/             the Mountebank oracle — not part of the Plane A contract
```

A runner puts this directory on its test classpath as one root. Paths inside feature files are
relative to it (`Given a fresh imposter from fixture "stateops/auto"` reads
`rift-fixtures/stateops/auto.json`).

`manifest.json` lists every feature file; `crates/rift-http-proxy/tests/plane_a_manifest.rs`
fails if the list and the disk disagree. `engineVersion` is `0.0.0-dev` here and stamped with the
real version when the tree is packaged.

## Running it locally

```bash
git clone https://github.com/EtaCassiopeia/rift-conformance
cd rift-conformance
export RIFT_BUILD_DIR=/path/to/staged/build    # rift, rift-lint, rift-verify + BUILD_INFO
RIFT_FEATURES_DIR=/path/to/rift/conformance RIFT_MAX_BODY_SIZE=10485760 \
  sbt lintFeatures engineConformance
```

`RIFT_BUILD_DIR` holds the three binaries under those names (the engine builds as
`rift-http-proxy`) plus a `BUILD_INFO` recording each one's `--version`; the "Stage binaries and
BUILD_INFO" step of [`.github/workflows/conformance.yml`](../.github/workflows/conformance.yml)
builds exactly that from `cargo build --release -p rift-http-proxy -p rift-lint`.

`RIFT_MAX_BODY_SIZE=10485760` is part of the contract: the size-cap scenarios in features 13 and 21
assert the `413` at a 10 MiB ceiling rather than sending bodies past the 512 MiB default. The
spawned engine inherits it.

`RIFT_FEATURES_DIR` is read when sbt loads the build: `reload` after changing it inside a session.
There is no feature 15: it specified the Redis flow store, which the engine removed in 0.22.0
(#1336), so the runner's Redis suite does not apply to this tree.

## Releases

Each release publishes `rift-conformance-<version>.tar.gz` (and `.sha256`), built by
[`scripts/gen-conformance-features.sh`](../scripts/gen-conformance-features.sh). It unpacks to
`rift-conformance-<version>/` holding this README, the stamped `manifest.json`, `features/`,
`rift-fixtures/`, `rift-lint-fixtures/` and `rift-scripts/` — the specification of exactly that
release, for running the runner against a released engine.

## Changing a scenario

Edit the feature or fixture in the same pull request as the engine change it describes. A step
the runner does not know yet needs a runner change in rift-conformance first. `RUNNER_REF` names
the runner revision the PR-time job (#1340) checks out; it tracks `main` today, so a new step is
usable once merged there, and pinning it to a commit is how to hold the runner still.
