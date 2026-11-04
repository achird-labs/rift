# Imposter lifecycle model

`mutants.py` here is model mutation: it edits the TLA+ and asks TLC. It is not cargo-mutants
(`.github/workflows/mutants.yml`).

`Lifecycle.tla` models how `ImposterManager` (`crates/rift-mock-core/src/imposter/manager.rs`) moves
an imposter generation through the port table, the datadir file, the load floor and the port-keyed
shared state, while a create, a delete, an `apply_config` sweep, an `apply_one` replace and a
`delete_all` race each other and requests are served.

```sh
JAR=/path/to/tla2tools.jar
java -cp "$JAR" tlc2.TLC -workers auto -deadlock -config LifecyclePR.cfg Lifecycle.tla
python3 witness.py "$JAR"
python3 mutants.py "$JAR"
python3 validate.py "$JAR" --selftest traces
```

Run TLC on a copy in a scratch directory, or it leaves `states/` here.

TLC's deadlock check is off (`-deadlock` disables it). Finished operations are terminal states, and
`Terminating` makes them stutter. `Liveness.cfg` covers a stuck operation instead: `OpsFinish` under
`FairSpec` says every operation returns. It holds at 27,212 distinct states, and a drain that never
ends fails it.

## The clock

The wall clock never advances in the model, because the floor exists for a re-create landing in the
second the old imposter last stamped (#1301). A plain request reads the stamp its content already
has. Only a content change, a stub write or a gated scenario transition, advances the clock
(`ContentChange`). The first version of this model advanced the clock on every request, which is
wrong, and an issue was filed from it before anyone caught that: see the correction on #1762.

## Actions and the code

Line numbers in the spec's comments are as of `5dc739a5`, before the probe points went in. Function
names here are the stable reference.

| Action | Code |
|:--|:--|
| `CreateBindBusy`, `CreateBindOk` | `bind_for_create`: the `contains` check, then the bind |
| `CreateBindBound` | not code: the model's generation bound refusing a create |
| `CreateReadFloor` | `create_imposter_staged`: `load_floors`, then `continue_load_clock` |
| `CreateSpawnAccept` | `spawn_accept_loops`: the loops run but accept nothing until the claim is won (#1764) |
| `CreateReread` | the winner re-reads `load_floors`, then `admit` opens the loops (#1762) |
| `CreateTryClaim` | `PortTable::try_claim`; the loser cancels and returns `PortInUse` |
| `CreateClaimStore` | not code: reachable only in the "no `mutations` lock" mutant |
| `CreatePersistLock`, `CreatePersistWrite` | `persist_imposter_with`: `persist_lock`, the `is_current` check, the write |
| `CreateRollback` | the failed-persist branch: `remove_if_same` under `persist_lock`, then cancel |
| `DeleteLock`, `DeleteLookup`, `DeleteUnlink` | `delete_imposter_inner`, the unlink path, under `persist_lock` |
| `ReplaceLock`, `ReplaceLookup` | `delete_imposter_inner`, the path a replace takes: the same lock, no unlink |
| `DeleteCancel` | `cancel.cancel()`: cuts requests still computing (#1643) |
| `DeleteClearShared` | `clear_shared_state`: `reset_scope`, `journal.clear`, `proxy_store.clear`, all by port, before the slot is freed (#1763) |
| `DeleteRecordFloor` | `record_load_floor`, after the cancel (#1762) |
| `DeleteRemoveIfSame` | `PortTable::remove_if_same`; `persist_lock` is released after it |
| `DeleteDrain` | joins the accept loops, drains connections within `conn_drain` |
| `DeleteFinish` | the delete returns |
| `ApplySnapshot`, `ApplySweep` | `apply_entries` and `delete_all`: the `ports()` snapshot, one delete per port |
| `ApplyEntry`, `ReplaceAfterDelete`, `ReplaceAfterCreate` | `apply_entry`, `replace_imposter` |
| `ReqStart`, `ReqCompute`, `ReqWrite` | a request: accepted, answered under its stamp and journalled, written |
| `ContentChange` | a stub write, or `stamp_scenario_transition` |

## What it checks

| Invariant | Meaning |
|:--|:--|
| `NoServeAfterDeleteReturns` | No byte is served by a generation after its delete returned (#596, #1643) |
| `FloorMonotone` | A later generation never serves at or below a stamp an earlier one served (#1301) |
| `RemoveOnlyOwn` | A delete or rollback vacates only the generation it means to (#1124) |
| `ErrDeleteStillServing` | `Err` from a delete means the imposter is still registered and serving |
| `ErrCreateNotRunning` | `Err` from a create means the imposter is not running (#563) |
| `LoserNeverAnswers` | A create that returned `PortInUse` answered nobody |
| `OneClaimant` | Two creates never both store into a slot |
| `SharedStateBelongsToCurrent` | A delete's clear never wipes the current generation's shared state |
| `ApplyReportsTruth` | An apply never reports as failed a port already in the state it asked for |
| `TableFileAgree` | When nothing runs, the table and the datadir name the same generation |
| `TableHoldsLive` | A registered generation is one that claimed its slot and was not cancelled |

## Configs

| Config | What races | Invariants left out |
|:--|:--|:--|
| `LifecyclePR.cfg` | two creates, one delete | |
| `Lifecycle.cfg` | two creates, two deletes, failing writes and unlinks | |
| `LifecycleApply.cfg` | an apply sweep, a `delete_all`, a delete, a create | `ApplyReportsTruth` |
| `LifecycleReplace.cfg` | an `apply_one` replace, a delete, a create | |
| `Mutants.cfg` | one create, two deletes: where a mutant's violation is its own | |

| Config | Distinct states | Depth | Wall time |
|:--|--:|--:|--:|
| `LifecyclePR.cfg` | 18,582 | 37 | 1 s |
| `Lifecycle.cfg` | 111,170 | 43 | 2 s |
| `LifecycleApply.cfg` | 567,048 | 45 | 4 s |
| `LifecycleReplace.cfg` | 252,034 | 46 | 2 s |

Times are from a laptop shared with other builds, so read them as an order of magnitude.

`-coverage 1` across these four cfgs hits every action except `CreateBindBound`, because no create
ran into the generation bound, and `CreateClaimStore`, which only the mutant reaches.

## Findings

Each was reproduced on the engine by an `#[ignore = "#n"]` test in
`crates/rift-mock-core/tests/lifecycle_interleavings.rs`, which holds the engine at a probe point to
force the order. Run them with `--ignored`. A fix removes the attribute and moves the invariant back
into the cfgs.

| Issue | Invariant | Defect |
|:--|:--|:--|
| #1774 | `ApplyReportsTruth` | An apply sweep reports a concurrent delete as failed |

`witness.py` requires each of these to stay reachable.

## Known limits, not filed

- **One late write after a clear.** A request already past its cancel check can still write to a shared
  journal, sequencer or store after `clear_shared_state`, or stamp after the floor is recorded, and the
  next imposter inherits it. `ReqCompute` is atomic against the cancel, so the model cannot see this.
  `with_request_journal` documents it.
- **A dropped create's rollback.** If its admin request is dropped while the rollback waits for
  `persist_lock`, it vacates the slot without the lock, so a delete already past its lookup could clear
  a newer imposter's state. It needs five things to coincide, and the model does not cover the drop:
  `DeleteRemoveIfSame`'s else-branch is reachable only this way.
- **A client too slow to read.** With `DrainTimesOut`, a response is written after the delete returned.
  The drain's bound is deliberate.
- **A stamp between the cancel and the end of the drain.** A request already past its cancel check can
  stamp after the floor was recorded and the next generation can reuse it. Holding a request at
  `Point::CasApplied` through a delete and a re-create delivered no response, because after the cancel
  a stamp reaches a client only if its response finishes in a poll that began before the cancel. That
  is one forced run, not a proof. The model cannot see it, and raising the next generation after the
  drain would neither close it in the model nor have a test that fails without it (#1762).
- **An in-process door during a create.** The `/__rift/` gateway and the front door reach an imposter
  through the table, not its gate, so between the claim and `CreateReread` they can serve the old stamp.
- **Two creates both bind.** `BindCheckSuffices` is violated by design: the claim decides.

## Witnesses and mutants

`witness.py` runs 5 cases, each requiring an exact verdict: the limits and findings above, plus two
controls. `mutants.py` puts fixed bugs back:

| Bug put back | Caught by |
|:--|:--|
| A delete returns before the drain (#596) | `NoServeAfterDeleteReturns` |
| The cancel leaves computing requests running (#1643) | `NoServeAfterDeleteReturns` |
| A delete removes whoever holds the port (#1124) | `RemoveOnlyOwn` |
| The file goes after the imposter (#1124) | `ErrDeleteStillServing` |
| The clear runs after the slot is vacated (#1763) | `SharedStateBelongsToCurrent` |
| The floor is recorded before the cancel (#1762) | `FloorMonotone` |
| The floor is not carried to the next generation (#1301) | `FloorMonotone` |
| The floor is not re-read after the claim (#1762) | `FloorMonotone` |
| The gate opens before the floor is re-read (#1762) | `FloorMonotone` |
| A rollback records no floor (#1762) | `FloorMonotone` |
| The accept loops answer before the claim (#1764) | `LoserNeverAnswers` |
| A failed persist leaves the imposter running (#563) | `ErrCreateNotRunning` |
| The rollback removes whoever holds the port (#1773) | `RemoveOnlyOwn` |
| The rollback does not take `persist_lock` (#1773) | `ErrDeleteStillServing` |
| `try_claim` without the `mutations` lock | `OneClaimant` |
| A drain that never ends | `OpsFinish`, on `Liveness.cfg` |

## Trace validation

`crates/rift-mock-core/src/imposter/probe.rs` defines the points; nothing installs a probe in
production. With `FORMAL_TRACE_DIR` set, `lifecycle_interleavings.rs` writes each run as ndjson.
`trace_labels.py` turns points into action labels, `validate.py` writes them as a `Trace` module, and
TLC searches `TraceValidation.tla` for a behaviour taking those steps. Accepted means `TraceNotDone`
was violated. A generated module avoids CommunityModules' `Json` and its TLC 1.8 dependency.

`traces/accept` holds recorded runs, including one forced race with two generations on a port.
`traces/reject` holds three runs edited by hand from a recorded one.

Replace and `apply_one` runs are not attributed: `labels()` returns `None`, and `validate.py` reports
them as skipped.

## What is outside the model

- Time, beyond the frozen wall clock. A real re-create more than a second later has no floor problem.
- One request per generation at a time: `req` holds one in-flight request.
- The serve-unbound path, HTTPS and rebinds.
- `restart_runtime_state`, the reload's route and intercept swaps, and two ports' interaction beyond
  a sweep's snapshot.
