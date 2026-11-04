# Formal models

`mutants.py` in these directories is model mutation: it edits the TLA+ and asks TLC. It is not cargo-mutants
(`.github/workflows/mutants.yml`).

TLA+ models of the engine's concurrent protocols. `docs/design/formal-verification.md` says why they exist and
what they found. Each directory's README maps the model's actions to the code and lists its findings and limits.

| Directory | Protocol |
|:--|:--|
| `lifecycle/` | Create, delete, replace and apply racing on a port |
| `proxyonce/` | Claiming, recording and forgetting a `proxyOnce` response |
| `scenario/` | Scenario transitions and their `Last-Modified` stamps |
| `scripting/` | The JS state cell, worker affinity and live state |
| `faultio/` | Arming a fault, write order and the lingering close |

## Running them

CI pins TLC 1.7.4 by checksum (`.github/workflows/formal.yml`).

```sh
curl -sSfLo tla2tools.jar https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar
JAR=$PWD/tla2tools.jar

python3 formal/check.py "$JAR" lifecycle pr     # the pull-request cfg
python3 formal/check.py "$JAR" lifecycle full   # the larger cfgs
python3 formal/lifecycle/witness.py "$JAR"      # limits and open defects stay reachable
python3 formal/lifecycle/mutants.py "$JAR"      # fixed bugs put back must be caught
```

`check.py` and the scripts run TLC on a copy in a temporary directory, which needs disk: a full cfg can write
gigabytes of state there.

## Files in a model

| File | Role |
|:--|:--|
| `*.tla` | The spec |
| `*PR.cfg` | The pull-request gate |
| other named cfgs | The larger runs, on master and nightly |
| `Known.cfg` | Invariants open defects violate, each naming its issue |
| `Mutants.cfg`, `Witness.cfg` | Bounds those scripts start from |
| `witness.py` | Each case requires an exact verdict |
| `mutants.py` | Each mutant must be caught by its named invariant |
| `TraceValidation.tla`, `validate.py`, `traces/` | Checks recorded engine runs against the model (`lifecycle`, `proxyonce`) |

## Reproducing a defect on the engine

`crates/rift-mock-core/tests/*_interleavings.rs` hold the engine at a probe point to force an order. A test marked
`#[ignore = "#n"]` states the correct behaviour and fails today:

```sh
cargo test -p rift-mock-core --all-features --test lifecycle_interleavings -- --ignored
```

With `FORMAL_TRACE_DIR` set, the lifecycle and proxyonce tests write their runs as ndjson for `validate.py`.
