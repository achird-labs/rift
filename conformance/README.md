# Rift conformance

Two trees, with opposite questions:

| Tree | Question it answers | Who runs it |
|---|---|---|
| [`features/`](features/) + fixtures | **What Rift must do.** The Plane A specification: Gherkin scenarios driving a real `rift`, `rift-lint` and `rift-verify` binary. | The Rust runner in [`runner/`](runner/): `cargo test -p rift-conformance-runner --test plane_a` |
| [`differential/`](differential/) | **What Mountebank does.** Every case is replayed on Mountebank 2.9.5 and on this build, and any difference not on its documented allow-list fails. | `cargo test -p rift-differential` (see its README) |

The specification lives in this repository so a behaviour change and the scenario that pins it land
in one pull request. Its runner lives here too: [`runner/`](runner/) (`rift-conformance-runner`,
cucumber-rs) runs every feature (`spec::PORTED_FEATURES`), and is the only runner. The Scala runner
in [rift-conformance](https://github.com/EtaCassiopeia/rift-conformance) that ran them before
(#1338 moved them over one batch at a time, #1346 to #1350) is retired.

## Layout (the contract a runner consumes)

```
conformance/
  README.md                 this file
  manifest.json             schemaVersion, engineVersion, and every feature file
  features/rift/NN-*.feature  one file per Plane A feature
  rift-fixtures/**          imposter documents, configfiles, TLS/intercept PEMs the scenarios load
  rift-lint-fixtures/*.json rift-lint rule fixtures (one per rule exercised)
  rift-scripts/*            Rhai/JS scripts for the scripting scenarios
  runner/                   the Rust Plane A runner (rift-conformance-runner) — not part of the contract
  differential/             the Mountebank oracle — not part of the Plane A contract
```

Paths inside feature files are relative to this directory (`Given a fresh imposter from fixture
"stateops/auto"` reads `rift-fixtures/stateops/auto.json`).

`manifest.json` lists every feature file; `crates/rift-http-proxy/tests/plane_a_manifest.rs`
fails if the list and the disk disagree. `engineVersion` is `0.0.0-dev` here and stamped with the
real version when the tree is packaged. There is no feature 15: it specified the Redis flow store,
which the engine removed in 0.22.0 (#1336).

## Running it

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
| `RIFT_MODE` | `external` (default), `embedded` or `container`: see [The three modes](#the-three-modes). |
| `RIFT_IMAGE` | Container mode's engine image, required there: unset or empty is a configuration error, never a skip and never a default image, so a run cannot quietly test an older engine. |
| `RIFT_CORPUS_ROOT` | The JSON corpus feature 01 lints, posts and verifies, and the spawned engine's working directory. Default: this repository's `sdk-conformance/corpus`. |
| `RIFT_FEATURES_DIR` | This tree, by default; point it at an unpacked `rift-conformance-<version>` to run a release's specification. |
| `RIFT_MAX_BODY_SIZE` | Part of the contract: the size-cap scenarios in features 13 and 21 assert the `413` at a 10 MiB ceiling rather than sending bodies past the 512 MiB default. A spawned engine inherits it, the embedded one gets it as `maxBodySize` and the container as `--max-body-size`. |

cucumber's own flags narrow a run: `cargo test -p rift-conformance-runner --test plane_a -- --name
'mutualAuth'` or `-- --tags '@core'` (one of the two, not both). They never reach past what the mode
can run. Each engine's output goes to a log directory printed at start; it is kept when the run
fails. The run ends with cucumber's `[Summary]` and the list of scenarios the mode left out, each
with its reason.

`cargo test -p rift-conformance-runner --lib` runs the runner's own unit tests (no engine needed).

Ports: every engine this runner spawns binds port 0 and reports the ports it bound, which the
runner reads from its startup lines (the intercept listener's from `GET /intercept`), so no port is
probed before an engine binds it. Every engine and imposter is addressed at `127.0.0.1`, never
`localhost`: macOS resolves `localhost` to `::1` first, where an unrelated process can hold the same
port while the engine listens on IPv4 only. A "fresh imposter" is posted without a `port` and the
runner reads the one the engine assigned back from the `201`. Only fixtures that name a port (`a
fresh verbatim imposter`, the corpus's 45xx imposters, the reload and front-door configs) bind a
fixed one. Feature 21's two imposters "on descending ports" take a fresh port for the first and,
for the second, the highest port below it that the engine accepts (a `400` saying the port is in use, whether
another imposter or another process holds it, moves one lower; a `500` is an engine fault and fails the run).
Feature 08's `PUT /imposters` scenarios replace the
shared engine's whole imposter set, which is safe because scenarios run one at a time and each
one's imposters are deleted after it.

### The shared engine

One engine serves the whole run. It runs with `--allow-injection` and an intercept listener seeded
with the committed test CA (`rift-fixtures/intercept/ca-cert.pem`, `ca-key.pem`), so feature 12 can
assert the CA it exports is that one. After each scenario its imposters are deleted, and its
intercept rules cleared when the scenario added any.

### Engines a scenario starts

Some scenarios start an engine of their own, stopped after the scenario:

| Tag (feature) | Engine |
|---|---|
| `@reload-engine` (11) | One serving a config file it reloads from, on the fixed ports 4870-4878 the fixtures name. |
| `@front-door` (11, 16) | The same, with `--front-door` on a port it picks. Feature 16's invalid route tables boot one once and assert on its exit code and output instead. |
| `@upstream-trust` (09) | `--upstream-ca-file` the test CA. |
| `@rift-debug-env` (05, 19) | `--debug`, holding the scenario's fixture; the flow-state steps address it while it runs. |
| `@admin-isolated` (21) | An empty one for `DELETE /imposters`, which on the shared engine would wipe every other scenario's imposters. |
| `@tls-flags` (20) | `--default-tls-cert`/`--default-tls-key`, `--no-self-signed-tls`, `--upstream-tls-skip-verify` or `RIFT_DISABLE_HTTP2=1`. A missing `--default-tls-cert` file is asserted on a boot-only engine's exit code and output. |
| `@intercept-runtime` (12) | One with no intercept listener, started and stopped over its admin API; one scenario's also without `--allow-injection`. |
| `@intercept-auth` (12) | An intercept listener requiring `Proxy-Authorization` (`--intercept-auth u:p`). |
| `@intercept-ws` (12) | An intercept listener whose WebSocket relay trusts the test CA upstream, relaying to a TLS echo origin the runner serves on the loopback. |

### The three modes

**`external`** (default) spawns the staged `rift` and runs every scenario.

**`embedded`** links `rift-ffi` and serves the real admin API in the test process (`rift_start` +
`rift_serve_admin`), as the SDKs embed it; the lint/verify tools and `BUILD_INFO` still come from
`RIFT_BUILD_DIR`. The intercept listener is started with `rift_start_intercept`, and the admin plane
serves the whole `/intercept*` surface against it. An engine a scenario starts is a second
in-process engine (`configFile`, `allowInjection: false`, `upstreamCaFile`, the intercept `auth`
option). It leaves out (`Mode::excludes`):

- `@upstream-trust` (09): it starts a second engine with `--upstream-ca-file`, which this harness
  only spawns in external mode;
- `@front-door` (11, 16): the front door is `rift --front-door`, which `rift_serve_admin` has no
  option for;
- `@rift-debug-env` (05, 19): debug mode is `RIFT_DEBUG`, which the engine reads once per process,
  so an in-process engine cannot have it for one scenario;
- `@tls-flags` (20): a TLS CLI flag or environment variable, which an in-process engine cannot take
  per scenario.

Known limit of embedded mode: the engine is linked into the test binary, so under `cargo test` it
runs at the debug profile, not the release build external mode spawns. The timing assertions are
lower bounds (`wait` behaviours), which a slower engine still meets, and one generous upper bound
(feature 03: a 200-250 ms wait range under a 2000 ms ceiling); a tighter bound or a timeout may need
`cargo test --release` in embedded mode.

**`container`** runs the image `RIFT_IMAGE`:

```bash
docker build -f crates/rift-http-proxy/Dockerfile -t rift-proxy:local .
RIFT_MODE=container RIFT_IMAGE=rift-proxy:local RIFT_BUILD_DIR=/path/to/staged/build \
  RIFT_MAX_BODY_SIZE=10485760 cargo test -p rift-conformance-runner --test plane_a
```

CI builds the image from the staged binary instead (`Dockerfile.prebuilt`, the job runs on
ubuntu-22.04 so that binary starts in the image's debian bookworm), and the release workflow runs
the same mode against the image it has just published (`docker-publish.yml`). The run is one
container, `rift-conf-<pid>-<n>` labelled `rift-conformance=1` (`docker ps --filter
label=rift-conformance` finds one an interrupted run leaked), started with `--port 2525
--allow-injection --metrics-port 4790 --intercept-port 8443` and the test CA (plus `--max-body-size`
from `RIFT_MAX_BODY_SIZE`), and removed with `docker rm -f` at the end, its `docker logs` written to
the log directory first.

- Every port is published on 127.0.0.1 only: the admin API runs with `--allow-injection`.
- Admin (2525), metrics (4790) and the intercept listener (8443) are published on free host ports,
  not 1:1: a developer machine often has Mountebank on 2525.
- The imposter ranges 4500-4599 and 4800-4899 are published 1:1. Fixtures and the corpus name ports
  in them (the corpus 4501-4521, fixtures 4800-4829 and 4870-4879), and the engine reports the port
  it bound, so host and container must agree.
- A fresh imposter takes its port from a round-robin pool over 4880-4899, clear of every fixture's
  fixed port. Only a `400` saying the port is already in use moves on to the next one; any other
  refusal fails the step. Engine assignment would land outside the published ranges, and probing the
  host finds nothing free (Docker's forwarder holds every published port).
- The corpus's `data/` is mounted read-only at `/data/data`; `/data` is the image's working
  directory, so a fixture's `data/...` path resolves as it does for a spawned engine. The test CA's
  directory is mounted read-only at `/intercept-ca`.

Container mode starts no engine but the shared one, and leaves out (`Mode::excludes`):

- every tag in [Engines a scenario starts](#engines-a-scenario-starts): `@reload-engine`,
  `@front-door`, `@upstream-trust`, `@rift-debug-env`, `@admin-isolated`, `@tls-flags`,
  `@intercept-runtime`, `@intercept-auth` and `@intercept-ws` (whose echo origin is on the host's
  loopback, which the container cannot reach either). Feature 11's `@no-config-source` scenario runs
  on the shared engine and still runs;
- `@full-corpus-verify` (01): `rift-verify --verify-dynamic` has the engine bind imposters on dynamic
  ports, which are not published;
- `@loopback-client` (22): Docker's forwarding rewrites the client address, so the engine never sees a
  loopback peer;
- `@connect-refused` (08, 21): a published port always accepts on the host;
- `@host-port` (08): an engine-assigned port lands outside the published ranges;
- `@tcp-rst` (03): Docker's forwarder turns the engine's RST into an orderly EOF.

The last five are inherent to Docker port publishing.

## Releases

Each release publishes `rift-conformance-<version>.tar.gz` (and `.sha256`), built by
[`scripts/gen-conformance-features.sh`](../scripts/gen-conformance-features.sh). It unpacks to
`rift-conformance-<version>/` holding this README, the stamped `manifest.json`, `features/`,
`rift-fixtures/`, `rift-lint-fixtures/` and `rift-scripts/`: the specification of exactly that
release, which `RIFT_FEATURES_DIR` points the runner at.

## Changing a scenario

Edit the feature or fixture in the same pull request as the engine change it describes. A step the
runner does not know yet is added to [`runner/tests/plane_a.rs`](runner/tests/plane_a.rs) in the
same pull request, its text matching the feature's byte for byte; a step that matches no function
fails the scenario.

Every feature must parse with the runner's parser (gherkin-rs):
`cargo test -p rift-conformance-runner --test features_parse` checks all of them (#1466). It is
stricter than official Gherkin in one place: a table cell accepts only the escapes `\n`, `\|` and
`\\`, and any other backslash fails the whole file. Write a literal backslash in a cell as `\\`
(`{\\"a\\":7}` is the cell `{\"a\":7}`).
