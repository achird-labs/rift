# Scenario transition model

`Scenario.tla` models scenario transitions and the `Last-Modified` stamps they issue, on one imposter, one flow and one
scenario. It checks the two claims of #1307 and #311: an answer never goes out under a stamp that outranks the transition
that replaced it, and a gated transition never overwrites a state its gate did not see. It also checks what a poller
depends on once a transition has finished, and three defects it found there are filed.

```sh
JAR=tla2tools.jar
java -XX:+UseParallelGC -cp $JAR tlc2.TLC -deadlock -workers auto -config ScenarioPR.cfg Scenario.tla  # PR gate
java -XX:+UseParallelGC -cp $JAR tlc2.TLC -deadlock -workers auto -config Scenario.cfg Scenario.tla    # full
python3 witness.py $JAR
python3 mutants.py $JAR
```

Run TLC on a copy in a temporary directory, so `states/` stays out of the repo. `-deadlock` turns TLC's deadlock check
off: every request runs once and finishes, so the end of a run is a state with no step, which is not an error here.

The engine side is `crates/rift-mock-core/tests/scenario_interleavings.rs`, which holds the real engine at the points in
`crates/rift-mock-core/src/imposter/probe.rs` to force an interleaving the model found. The tests marked
`#[ignore = "#n"]` state the correct behaviour and fail today:

```sh
cargo test -p rift-mock-core --all-features --test scenario_interleavings -- --include-ignored --test-threads=1
```

## Actions and the code

Each action is one step between two points where another task can run. No lock is held across steps, and the worker
threads run in parallel, so one request's step can land between any two of another's. Lines are in
`crates/rift-mock-core/src/imposter/core/matching.rs` unless named.

| Action | Code | Probe point |
|:--|:--|:--|
| `Copy` | `imposter.scenario_transitions()`, `handler.rs:1359`, before any gate is read | `TransitionsCopied` |
| `GateRead` | one `flow_store.get` per gated candidate, `:181-194`; a match ends the scan at `:221`, none is `:230` | `GateRead` |
| `CasCompare` | `compare_and_set` expecting the gate's state, `:686`; on `Conflict(None)` for the initial state, again expecting absence, `:692-695`; else dropped, `:698`, `:705`. One step when `AtomicCas`: `backends/inmemory.rs:287`, under the store's write lock from `:294`. Otherwise the default's `get` and compare, `extensions/flow_state.rs:107-108` | `CasApplied`, `CasConflict` |
| `CasSet` | the default's `set`, `extensions/flow_state.rs:109`; only when `AtomicCas = FALSE` | |
| `Issue` | `load_clock.next()`, `:577`; one atomic update, `core/mod.rs:169` | `TransitionStampIssued` |
| `Publish` | `scenario_transitions.rcu`, `:580-584`; retries until it lands, then the entry is the stamp `Issue` took | `TransitionStamped` |
| `Serve` | `served_load_stamp_unprobed`, `:629-650`, from `handler.rs:2066` and `:2416`, over the copy | `Served` |
| `SetRead`, `SetWrite` | `set_scenario_state`: `changed` at `:537-540`, the plain write at `:542`, the stamp at `:547` | `ScenarioChangeRead` |
| `DelRead`, `DelWrite` | `flow_delete` through `ScenarioStampingStore::stamped`: `would_change` at `:990`, `:778`, decided at `:955`, the write at `:960`, the stamp at `:961` | `ScenarioChangeRead` |

## What it checks

| Property | Cfg | Verdict | Meaning |
|:--|:--|:--|:--|
| `NeverOldAnswerUnderNewStamp` | PR, full | holds | An answer's stamp is below the stamp of any later write that moved away from it (#1307). It rests on the clock being strictly increasing and on the copy coming before the gate read. |
| `TransitionsAreCas` | PR, full | holds | A gated transition never writes over a value it did not expect (#311). |
| `StampMonotone` | Known | violated, #1779 | The stamp map's entry never falls. |
| `NoStaleAnswerAtRest` | Known | violated, #1779, #1780 | With no stamp in flight, a client holding another answer is told so: its stamp is below the current one. The window between a write and its publish is excluded on purpose: there a client gets `304` until the publish, which ends it. |
| `NoMatchOnlyWhenUncovered` | Known | violated, #1781 | Every state has a stub in this table, so a scan never matches nothing. |

## Witnesses

`witness.py` requires each of these verdicts exactly.

| Case | Expected |
|:--|:--|
| `Witness.cfg`: two transitions racing | `NeverDroppedTransition` violated: a lost compare-and-set is normal concurrency, and the properties above would be vacuous without it |
| The same with `AtomicCas = FALSE` | `TransitionsAreCas` violated: the trait's default overwrites a state it did not expect |
| The same with `AtomicCas = TRUE` (control) | no error |
| `Known.cfg`, each invariant alone | violated |
| Each finding alone, on the fewest requests that show it | violated |
| The same, under the fix its issue proposes | no error |
| `ScenarioPR.cfg` (control) | no error |

## Mutants

`mutants.py` runs each on `Mutants.cfg`, and requires its named invariant to be violated and the rest of the gate to pass.

| Bug | Caught by |
|:--|:--|
| The stamp goes out before the store write (#1307) | `NeverOldAnswerUnderNewStamp` |
| A gated transition is a plain `set` (#311) | `TransitionsAreCas` |
| The stamps are copied after the gate read (#1307) | `NeverOldAnswerUnderNewStamp` |

## Findings

Each was reproduced on the real engine by a test holding it at a probe point, on the in-memory store. The hold forces the
interleaving. None was observed without it, and how often each occurs under load was not measured.

| Issue | Defect | Test in `scenario_interleavings.rs` |
|:--|:--|:--|
| #1779 | A transition stalled between `Issue` and `Publish` puts a lower stamp over a higher one. A client on `If-Modified-Since` stays on `304`, until the next transition | `a_stalled_stamp_does_not_take_the_scenario_back` |
| #1780 | A set or delete that decided "no change" can still move the state, unstamped, with the same result. Reproduced for `set_scenario_state` and `flow_delete`; four more sites of the same shape are traced in the source only, and the issue lists them | `a_set_that_decided_no_change_still_stamps_what_it_moved`, `a_delete_that_decided_no_change_still_stamps_what_it_moved` |
| #1781 | A scan reads the state once per gate, so a transition during it gives a no-match though every state has a stub | `a_scan_of_covered_states_finds_a_stub_while_the_state_moves` |

`If-None-Match` takes precedence over `If-Modified-Since` (`conditional.rs:155`) and compares the bytes, so a client
sending an ETag does not see the stale `304`s.

On fixing one, remove its `#[ignore]`, move its invariant from `Known.cfg` to `ScenarioPR.cfg` and `Scenario.cfg`, and
delete its row from `witness.py`. `NoStaleAnswerAtRest` moves once both #1779 and #1780 are fixed.

### Not filed

- **The default `compare_and_set` is not atomic.** The model shows what it does (`TransitionsAreCas`, `AtomicCas = FALSE`),
  and no shipped path reaches it. `InMemoryFlowStore` overrides it, atomic under its write lock. `MeteredFlowStore` and
  `ScenarioStampingStore` override it and forward. rift-cluster's `ClusteredFlowStore` overrides it
  (`rift-cluster/src/stores/flow.rs:2025`), as a `WriteOp::Cas` the owner applies under its `rmw` lock (`:442`, `:670`, `:813`).
  `NoOpFlowStore` inherits the default, but is never selected for an imposter (`flow_state.rs:167`) and keeps no state. So
  only a third-party `FlowStore` can hit it, and the trait's comment already says so (`flow_state.rs:97-99`, #311). It stays
  here as a documented limit.
- **A transition landing during a delete of the imposter** is #1762, from the lifecycle model. This model has no delete.

## Known limits

- One flow and one scenario. `served_load_stamp` folds several scenarios with a maximum, and the map is keyed by scenario
  and not by flow, so a transition in one flow costs a poller in another an extra `200`. Neither is modelled.
- All stubs are conditional and load before the run, so `is_conditional_gate` is always true and a stub's own stamp never
  decides. Stub edits during a run are out: `restamp` (#1303) and the snapshot `is_conditional_gate` reads.
- A self-transition (`requiredScenarioState` equal to `newScenarioState`) is not in the stub table, so the `changes` guard
  (`:676`) is not exercised here. It has engine tests under #1307.
- Backend errors are out: a failing store fails the request (#318), with no stamp.
- A setter's answer is not modelled: its ungated stub's body does not depend on the state.
- The no-match interceptor's retry (`handler.rs:1389-1418`) is not modelled.
- There is no trace validation here, as there is in `formal/lifecycle`. The probe points are used for holds.

## Runs

Measured on this model, on a laptop shared with other jobs. Times are wall clock.

RUNTABLE
