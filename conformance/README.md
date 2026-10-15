# Rift conformance

Two trees, with opposite questions:

| Tree | Question it answers | Who runs it |
|---|---|---|
| [`features/`](features/) + fixtures | **What Rift must do.** The Plane A specification: Gherkin scenarios driving a real `rift`, `rift-lint` and `rift-verify` binary. | The Rust runner in [`runner/`](runner/) for the features it has ported; the [rift-conformance](https://github.com/EtaCassiopeia/rift-conformance) runner, with `RIFT_FEATURES_DIR` pointed here, for the rest |
| [`differential/`](differential/) | **What Mountebank does.** Every case is replayed on Mountebank 2.9.1 and on this build, and any difference not on its documented allow-list fails. | `cargo test -p rift-differential` (see its README) |

The specification lives in this repository so a behaviour change and the scenario that pins it land
in one pull request. Two runners read this tree while the features move from one to the other
(#1338, #1346): the Rust runner in [`runner/`](runner/) (cucumber-rs) runs the features in its
`spec::PORTED_FEATURES` (today 00, 01, 02, 06, 07, 09, 11, 14, 17 and 18), and the Scala runner (zio-bdd) in rift-conformance
runs every other one. Each feature has exactly one runner: the Scala runner drops the files listed
in its `conformance/ported.txt` (rift-conformance#244), and a porting batch updates both lists in the
same release. The container mode is the exception: the Scala runner's container leg keeps the
ported features until it drops them there, now that the Rust runner has a container mode (#1467).

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
  runner/                   the Rust Plane A runner (rift-conformance-runner) — not part of the contract
  differential/             the Mountebank oracle — not part of the Plane A contract
```

A runner puts this directory on its test classpath as one root. Paths inside feature files are
relative to it (`Given a fresh imposter from fixture "stateops/auto"` reads
`rift-fixtures/stateops/auto.json`).

`manifest.json` lists every feature file; `crates/rift-http-proxy/tests/plane_a_manifest.rs`
fails if the list and the disk disagree. `engineVersion` is `0.0.0-dev` here and stamped with the
real version when the tree is packaged.

## Running it locally

### The Rust runner (the ported features)

Build and stage the binaries the way CI does: `rift`, `rift-lint` and `rift-verify` in one
directory, plus a `BUILD_INFO` recording each one's `--version` (the "Stage binaries and BUILD_INFO"
step of [`.github/workflows/conformance.yml`](../.github/workflows/conformance.yml), runnable as is
on Linux; on macOS use `shasum -a 256` for `sha256sum`). Then:

```bash
cargo build --release -p rift-http-proxy -p rift-lint --bins
# ... stage target/release/{rift-http-proxy as rift, rift-lint, rift-verify} + BUILD_INFO ...
RIFT_BUILD_DIR=/path/to/staged/build RIFT_MAX_BODY_SIZE=10485760 \
  cargo test -p rift-conformance-runner --test plane_a
```

| Variable | Meaning |
|---|---|
| `RIFT_BUILD_DIR` | The staged build. Unset, the test prints why and passes with no scenario run, so a plain `cargo test --workspace` stays green. |
| `RIFT_CONFORMANCE_REQUIRE` | Any value: a missing `RIFT_BUILD_DIR` fails instead of skipping, and so does a run in which no scenario ran. CI sets it. |
| `RIFT_MODE` | `external` (default): spawn the staged `rift`. `embedded`: link `rift-ffi` and serve the real admin API in the test process (`rift_start` + `rift_serve_admin`), as the SDKs embed it; the lint/verify tools and `BUILD_INFO` still come from `RIFT_BUILD_DIR`. `container`: `docker run` the image `RIFT_IMAGE` (see [Container mode](#container-mode)); the tools and `BUILD_INFO` still come from `RIFT_BUILD_DIR`. |
| `RIFT_IMAGE` | Container mode's engine image, required there: unset or empty is a configuration error, never a skip and never a default image, so a run cannot quietly test an older engine. |
| `RIFT_CORPUS_ROOT` | The JSON corpus feature 01 lints, posts and verifies, and the spawned engine's working directory. Default: this repository's `sdk-conformance/corpus`, which has 15 imposters where the Scala runner's vendored `corpus/` has 16 (`21-behaviors-array-copy.json` is not in it), so the two runners' feature 01 do not cover exactly the same files. |
| `RIFT_FEATURES_DIR` | This tree, by default; point it at an unpacked `rift-conformance-<version>` to run a release's specification. |
| `RIFT_MAX_BODY_SIZE` | As below; the embedded engine gets it as `maxBodySize`. |

cucumber's own flags narrow a run: `cargo test -p rift-conformance-runner --test plane_a -- --name
'mutualAuth'` or `-- --tags '@core'`. They never reach past the ported features or what the mode
can run. Each engine's output goes to a log directory printed at start; it is kept when the run
fails.

Ports: the engine's admin and metrics ports are chosen by the OS (bind `:0`, release, pass to the
engine; a failed start retries on fresh ports). A "fresh imposter" is posted without a `port` and
the runner reads the one the engine assigned back from the `201`, so there is no probe-then-bind
race. Only fixtures that name a port (`a fresh verbatim imposter`, the corpus's 45xx imposters)
bind a fixed one.

Feature 11's `@reload-engine` scenarios each start an engine of their own serving a config file (on
the fixed ports 4870-4878 the fixtures name), stopped after the scenario: a spawned `rift
--configfile` in external mode, a second in-process engine given the file as `configFile` in
embedded mode. Container mode does not start one and leaves them out.

External mode runs every ported scenario. Embedded mode leaves out (`Mode::excludes`; the run lists
what it left out):

- feature 09's `@upstream-trust` scenario: it starts a second engine with `--upstream-ca-file`, and
  only external mode spawns engines;
- feature 11's three `@front-door` scenarios: the front door is `rift --front-door`, which
  `rift_serve_admin` has no option for. One of the three (the warning for a routes block without
  `--front-door`) would run, but carries the tag.

Unlike the Scala runner's embedded leg, this one runs `@metrics-port` (the embedded engine serves
its own metrics listener) and `@no-config-source` (its shared engine has no config file).

#### Container mode

```bash
docker build -f crates/rift-http-proxy/Dockerfile -t rift-proxy:local .
RIFT_MODE=container RIFT_IMAGE=rift-proxy:local RIFT_BUILD_DIR=/path/to/staged/build \
  RIFT_MAX_BODY_SIZE=10485760 cargo test -p rift-conformance-runner --test plane_a
```

CI builds the image from the staged binary instead (`Dockerfile.prebuilt`, the job runs on
ubuntu-22.04 so that binary starts in the image's debian bookworm). The run is one container,
`rift-conf-<pid>-<n>` labelled `rift-conformance=1` (`docker ps --filter label=rift-conformance`
finds one an interrupted run leaked), started with `--port 2525 --allow-injection --metrics-port 4790` (plus
`--max-body-size` from `RIFT_MAX_BODY_SIZE`: the container does not inherit the variable) and removed
with `docker rm -f` at the end, its `docker logs` written to the log directory first.

- Every port is published on 127.0.0.1 only: the admin API runs with `--allow-injection`.
- Admin (2525) and metrics (4790) are published on free host ports, not 1:1: a developer machine
  often has Mountebank on 2525.
- The imposter ranges 4500-4599 and 4800-4899 are published 1:1. Fixtures and the corpus name ports
  in them (the corpus 4501-4521, fixtures 4800-4829 and 4870-4879), and the engine reports the port
  it bound, so host and container must agree.
- A fresh imposter takes its port from a round-robin pool over 4880-4899, clear of every fixture's
  fixed port. Only a `400` saying the port is already in use moves on to the next one; any other
  refusal fails the step. Engine assignment would land outside the published ranges, and probing the
  host finds nothing free (Docker's forwarder holds every published port).
- The corpus's `data/` is mounted read-only at `/data/data`; `/data` is the image's working
  directory, so a fixture's `data/...` path resolves as it does for a spawned engine.

Container mode starts no engine but the shared one, and leaves out (`Mode::excludes`; the run
lists what it left out):

- `@upstream-trust` (09) and `@front-door`: they start an engine of their own;
- `@reload-engine` (11): every scenario that starts an engine of its own serving a config file.
  Feature 11's `@no-config-source` scenario runs on the shared engine and still runs;
- `@full-corpus-verify` (01): `rift-verify --verify-dynamic` has the engine bind imposters on dynamic
  ports, which are not published;
- `@loopback-client`: Docker's forwarding rewrites the client address, so the engine never sees a
  loopback peer;
- `@connect-refused`: a published port always accepts on the host;
- `@host-port`: an engine-assigned port lands outside the published ranges;
- `@tcp-rst`: Docker's forwarder turns the engine's RST into an orderly EOF.

The last four are inherent to Docker port publishing; no ported feature carries them yet, and the
features that do inherit the exclusion when they are ported.

Known limit of embedded mode: the engine is linked into the test binary, so under `cargo test` it
runs at the debug profile, not the release build external mode spawns. The timing assertions ported
so far are lower bounds (`wait` behaviours), which a slower engine still meets; an upper bound or a
timeout may need `cargo test --release` in embedded mode.

### The Scala runner (every other feature)

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

Edit the feature or fixture in the same pull request as the engine change it describes. In a
ported feature, a step the Rust runner does not know yet is added to
[`runner/tests/plane_a.rs`](runner/tests/plane_a.rs) in the same pull request; in any other feature
it needs a runner change in rift-conformance first. `RUNNER_REF` names
the runner revision the PR-time job (#1340) checks out; it tracks `main` today, so a new step is
usable once merged there, and pinning it to a commit is how to hold the runner still.

Every feature must parse with the Rust runner's parser (gherkin-rs), ported or not:
`cargo test -p rift-conformance-runner --test features_parse` checks all of them (#1466). It is
stricter than official Gherkin in one place: a table cell accepts only the escapes `\n`, `\|` and
`\\`, and any other backslash fails the whole file. Write a literal backslash in a cell as `\\`
(`{\\"a\\":7}` is the cell `{\"a\":7}`). gherkin-rs and zio-bdd agree on `\\"`, but not on every
sequence (zio-bdd turns `\\n` into a newline), so check a new escape against both runners.
