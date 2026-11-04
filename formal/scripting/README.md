# Script state cell model

`StateCell.tla` models the Mountebank-style script state of one imposter: its cell (`ScriptStateCell`), the pool's route
of its runs to one worker (`MbPool::claim`, #1678), the live JS `state` object those runs share there
(`MbRealm::live_state`), and the swap a runtime restart makes (#1726). It checks the claim #1678 and #1726 make together:
the runs of an imposter see one state, the cell is current whenever a caller holds an answer, and a run across a restart
writes where it started.

```sh
J=~/.vscode/extensions/tlaplus.vscode-ide-*/tools/tla2tools.jar
java -cp $J tlc2.TLC -workers auto -config StateCellPR.cfg StateCell.tla   # the PR gate
java -cp $J tlc2.TLC -workers auto -config StateCell.cfg StateCell.tla     # master
java -cp $J tlc2.TLC -workers auto -config Live.cfg StateCell.tla          # under fairness
python3 witness.py $J
python3 mutants.py $J
```

TLC's deadlock check stays **on**: a state where a run is stuck with nothing enabled is an error here (`Terminated` is
the only allowed end). Run a copy in a temp directory, so `states/` stays out of the repository. Line numbers below are
`crates/rift-mock-core/src/scripting/js_engine.rs` unless a file is named; each action in the spec cites its own.

## The steps and the code

| Action | Code |
|:--|:--|
| `Capture` | `ImposterCore::script_scope` (`imposter/core/mod.rs:1187`), an atomic load, made per hook call site (`core/matching.rs:148`, `handler.rs:1489`, `:1589`, `:1632`, `:2273`) |
| `Submit` | `submit_mb_job` (`:1119`), `MbPool::claim` (`:965`): the key's worker while it has a claim, else any |
| `CallerLeaves` | the caller's `tokio::time::timeout` (`:2570`) or its cancellation drops the oneshot: `Reply::abandoned` |
| `StartAbandoned`, `Defer`, `Start` | `start_inject` (`:2701`), `start_decorate` (`:4149`), `start_predicate_inject` (`:3071`): the abandoned check, `split_from` (`:416`), `live_state` (`:528`), the hook function, `admit` (`:561`) |
| `OwnJob`, `InheritedJob` | `MbRealm::step` (`:579`), one job. It writes. A throw ends the run it is tagged with if the run queued it, and is logged and dropped if the run inherited it (#1786) |
| `Finish`, `Deadline`, `Abandoned`, `Idle` | `MbRealm::ending` (`:612`) |
| `EndSave`, `FinishAnswered`, `EndRelease` | `MbRealm::end` (`:659`) in its order: `save_state` (`:674`), `hook.finish` (`:679`), `retag` and drops (`:686-:697`) |
| `Panic` | `run_mb_worker` (`:1094-:1105`) |
| `Restart` | `restart_runtime_state` (`imposter/manager.rs:3083`). A delete and a same-port re-create is the same step: a new cell |
| `SetAccess` | `set_script_file_access` (`imposter/core/mod.rs:765`) |

A worker's steps never interleave with each other: it is one thread. The model holds this with `busy` for the three steps
of an end, and takes a synchronous section (the hook function with its seed) as one step. Other workers do interleave with
each step of an end, which is what lets `the claim is released before the save` be seen at all.

## What it checks

Every run writes at start and at every job. A run that writes nothing adds no behaviour these properties can see: each is
of the form "these writes are in that set", so more writes can only find more violations. For the same reason a script
that deletes a key is not modelled, and a state is a growing set of write tokens; a save replaces the cell with the live set,
as `*imposter_state = map` does.

| Property | Kind | Meaning |
|:--|:--|:--|
| `NeverTwoWorkersOneKey` | invariant | The control: all the runs holding a claim on a cell are on one worker. |
| `ClaimsAgree` | invariant | A claim is held from the submit to the run's end, and the route names the worker. |
| `OneLiveStatePerKey` | invariant | A cell has at most one live state, across every worker and realm. |
| `OldRunSavesToOldCell` | invariant | A cell holds only writes of runs scoped to it (#1726). |
| `CellCurrentBeforeAnswer` | invariant | A run's writes are in its cell when its caller has the answer (#1678). |
| `NoLostUpdate` | invariant | Every write executed is in the live state while there is one, else in the cell. |
| `EndedRunsLeaveNothing` | invariant | An ended run has no job left, holds no claim, and holds no worker. |
| deadlock | TLC's check | Every state has a step, or every run ended. |
| `RunsEnd` | under fairness | Every run submitted ends. Needs the `MB_ABANDON_POLL` wake-up (below). |

## Heirs, abandonment, deferral and the poll

All four are modelled.

- **Heirs.** `EndRelease` passes what a finished or idle run left to any kin in flight (the code takes the first in start
  order; any is more orders). A predicate never bequeaths. The inherited expiry (`retag`'s deadline) is left out: a job dropped
  at its expiry gives a state a run with fewer inherited jobs reaches anyway.
- **Abandonment.** `CallerLeaves` then `Abandoned` or `StartAbandoned`. Abandoned ends save, as the code does, and give no
  answer.
- **Realm deferral.** `Defer` keeps the claim and `Start` retries, on `SetAccess` changing the access under a run in flight.
  The realm's LRU eviction (`:424`) is left out. It removes only a realm with no run in flight (`:433`), and such a realm has no
  live state, since `save_state` drops it with the last run, so nothing the properties read changes.
- **`MB_ABANDON_POLL` (`:403`).** The model has no clock. Every timed end (a deadline, a timer, a look for an abandoned run)
  may happen at any step, so the 100 ms is covered for safety by being allowed at every moment. For liveness, the poll
  is what justifies the weak fairness on `Finish`/`Deadline`: `next_wake` (`:462`) never returns later than the poll
  when a run is in flight, and `has_runs` counts deferred starts (`:443`), so a worker with work never sleeps forever.

## What leaving things out hides

- **A script keeping a state object in a global.** A realm's globals are shared by design (`MbJsThread`'s doc), so
  `globalThis.s = config.state` keeps a reference to a live object after the last run dropped it; a later write through it
  reaches nobody. The model's live state has no aliases. Not checked here.
- **State that is not JSON.** A promise or a function in `state` lives in the live object and is gone once the last run saved:
  `js_to_json` keeps JSON (#1635). The model treats every write as savable, except for `SaveFails`.
- **Jobs dropped at the answer with no heir** (`:696`). A write the job would have made never executes, so it is not a lost
  update here. Mountebank's timers would still fire. Deliberate: a job never outlives its run's deadline (#1645).
- **A CPU-bound script.** A synchronous section is one step; how long it holds a worker is not in the model.
- **`predicateGenerators.inject`** (`with_mb_js_thread`, `:1157`): it has no state and no key, and runs on the shared queue.
- **The `StateKey` being an address.** The model's keys are unique by construction; "Refuted" below says why the code's are.

## Witnesses

`witness.py` requires exactly these verdicts:

| Witness | Expected |
|:--|:--|
| A run in flight across a restart saves to the old cell | `NeverSaveToOldCell` violated |
| Runs of one imposter on two workers, once a restart made two keys | `NeverTwoWorkersOneImposter` violated |
| A start waits for another realm's runs | `NeverDeferred` violated |
| A run inherits a finished kin's jobs | `NeverInherits` violated |
| A run is dropped once its caller left | `NeverAbandoned` violated |
| A state `js_to_json` refuses (#1635, documented) | `NoLostUpdate` violated, with `SaveFails` |
| A panicking job loses its worker's unsaved writes (documented, `:1079`) | `NoLostUpdate` violated, with `Panics` |
| `StateCellPR.cfg`, including the control `NeverTwoWorkersOneKey` | no error |
| `Witness.cfg`: refused saves, panics and answer getters on, and affinity, one live state and the cells still hold | no error |

## Mutants

`mutants.py` puts each bug in. Each is caught by the one invariant named, first among those in the cfg:

| Bug | Caught by |
|:--|:--|
| The answer goes out before the save (#1678) | `CellCurrentBeforeAnswer` |
| A run saves to the imposter's current cell (#1726) | `OldRunSavesToOldCell` |
| The claim is released before the save | `OneLiveStatePerKey` |
| No deferral across realms | `OneLiveStatePerKey` |
| The live state goes with any run's end (#477) | `NoLostUpdate` |
| A run goes to any worker | `NeverTwoWorkersOneKey` |
| A run's jobs outlive it (`EndedRunsLeaveNothing` off, which would catch it structurally) | `NoLostUpdate` |
| A run re-enters on a fresh `MbJsThread` | `NoLostUpdate` |

## Findings

Each was reproduced on the engine by an `#[ignore]`d test in `crates/rift-mock-core/tests/scripting_interleavings.rs`, which
fails today and states the correct behaviour. `Known.cfg` holds the violated invariants until the fix lands.

| Issue | Finding | Invariant | Verified by |
|:--|:--|:--|:--|
| #1784 | A restart between a request's hooks splits it across two cells: the inject does not see what the predicate wrote | `RequestKeepsOneCell` | Test: holds the predicate run at its save, restarts; body `undefined`, not `seen` |
| #1785 | A getter on the answer runs after the save, so its write is lost with no other run in flight | `CellCurrentBeforeAnswer`, `NoLostUpdate` with `GetterWrites` | Test: second answer `undefined`, not `1`. Mountebank 2.9.5, run with the same script, answers `1` |

Passing tests in that file put the model's other claims on the real engine: an end's order (`seeded, saved, finished,
released`); a run held at its save keeps the imposter's next run behind it on one worker, with idle workers free; a run across
a restart saves to its old cell and the next run does not see it; a start in another realm waits and sees what was saved.

## Refuted

- **A reentrant `run_blocking` losing updates.** It would: a run on a fresh `MbJsThread` seeds from the cell, beside the
  live state, and the outer run's save overwrites it (the last mutant shows this). But it cannot happen for production code.
  `IS_MB_WORKER` is true only on a pool thread, and code runs there only through a script: the entry points that call
  `run_blocking`/`with_mb_js_thread` (`execute_mountebank_inject`, `execute_predicate_inject`,
  `execute_mountebank_decorate_within`, `execute_predicate_generator_inject`) are called from
  `imposter/response.rs:618`, `imposter/predicates/mod.rs:765` and `imposter/core/proxy.rs:977`, none reachable from a
  native a Mountebank-style script can call. Those natives are the logger and `console` (`log_script_event`: tracing only),
  `require` (`require_impl`: reads and evaluates a file) and the timers (`js_runtime.rs`). `ctx`, `http` and the like belong to
  `_rift.script`'s contexts, on other threads. The branch serves tests and a future native; a native that re-entered
  would need to reach the live state instead.
- **A stale live state under a reused address.** `StateKey` is the cell's address. A freed cell's address could return for a new
  one. It cannot alias a live state: the entry goes in `save_state`, before `end` drops the scope. Two drop orders do let the
  scope go before the claim (`MbRun::begin` failing in `start_*`, and `InFlight`'s fields when a panic drops a realm), which
  could at most route a new key to the same worker for an instant. Traced in the source, no test.
- **A worker sleeping past a deferred start.** `has_runs` counts `deferred` (`:443`), and `step` retries them (`:449`).

Unconfirmed: nothing. Every counterexample TLC gave was either reproduced above or an error in the model, which was fixed.

## Runs

TLC 2026.11.02, 4 workers, on a machine shared with other runs (2026-11-03). All with TLC's deadlock check on.

| Cfg | Bounds | Verdict | Generated | Distinct | Depth | Time |
|:--|:--|:--|--:|--:|--:|--:|
| `StateCellPR.cfg` | 2 runs, 2 workers, 2 accesses, 1 restart, 1 access change, 1 job per run | no error | 1,536,302 | 760,890 | 21 | 28 s |
| `StateCell.cfg` | the same with 3 runs, under symmetry of workers, runs and accesses | no error | 42,350,300 | 18,563,719 | 30 | 9 min 36 s |
| `Live.cfg` (`RunsEnd`) | 2 runs, no restart, no symmetry | no error | 322,200 | 192,992 | 20 | 1 min 15 s |
| `Witness.cfg`, `-coverage 1` | 2 runs, with `SaveFails`, `Panics`, `GetterWrites` | no error on the invariants that must survive them | 9,879,830 | 3,960,436 | 22 | 3 min 23 s |

Every action has hits on `Witness.cfg`, none at zero, including `Defer` (387 distinct) and `InheritedJob` (2,693 distinct).
`Terminated` adds no state by design: it is the stutter at the end. `StateCellPR.cfg` shows `Panic` unhit, as its flags are off.

An earlier, larger `Live.cfg` with a restart ran out of disk at a million distinct states: TLC's liveness graph is on disk. The
restart is off there because of this, so `RunsEnd` is not checked across a swap; a swap only adds a second key, on which the same
steps apply.

`witness.py` and `mutants.py` exit 0.
