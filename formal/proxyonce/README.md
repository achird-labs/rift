# proxyOnce claim model

`ProxyOnce.tla` models how requests with one signature, stub edits and store clears move through the
`proxyOnce` claim protocol of one imposter: `handle_proxy_request` and `HeldClaim`
(`crates/rift-mock-core/src/imposter/core/proxy.rs`), `LocalProxyStore`
(`recording/proxy_store.rs`), and `mutate_stubs_from` with `forget_orphaned_claims` (`core/mod.rs`).
It checks that one miss makes one recording, that a claim cannot wedge, and that a recording lives as
long as its stub.

```sh
JAR=tla2tools.jar                        # from the releases of tlaplus/tlaplus
java -cp $JAR tlc2.TLC -deadlock -config ProxyOncePR.cfg ProxyOnce.tla   # the PR gate
java -cp $JAR tlc2.TLC -deadlock -config ProxyOnce.cfg ProxyOnce.tla     # the full run, three requests
java -cp $JAR tlc2.TLC -deadlock -config ProxyOnceTwoEdits.cfg ProxyOnce.tla   # the full run, two edits
python3 witness.py $JAR
python3 mutants.py $JAR
python3 validate.py $JAR --selftest traces
```

TLC's own deadlock check is off: every request and edit runs once, so all behaviours end. Run copies
in a temp directory to keep `states/` out of the repo; `formal/tlacheck.py` does.

## Actions and the code

Each action is one step between two points where another task can run. Requests run on tokio's
multi-threaded runtime, so synchronous code overlaps too: the boundary is a released lock or a lock-free
load, not only an `.await`. Line numbers are of the commit that added this model.

| Action | Code | Why it is one step |
|:--|:--|:--|
| `Match` | `handler.rs:1502`; snapshot load `core/mod.rs:789` | One wait-free load. A recorded stub answers from the snapshot and never reaches the store |
| `Claim`, `Reclaim` | `try_claim_as`, `proxy_store.rs:375-391`; called at `proxy.rs:1405`, `:1423` | The `responses` read guard is held across the `pending` lock (#171/#118) |
| `ShadowCheck` | `shadowed_recordings`, `core/proxy.rs` | Its own snapshot load, after the claim; the tokens of every carrier behind the answering stub |
| `ShadowForget` | `forget_recorded_by`, `core/proxy.rs`, `proxy_store.rs` | One write lock; removes the entry only if one of those tokens settled it, leaves `pending` alone |
| `Lookup` | `proxy.rs:1446`, nothing found `:1456-1464` | One read lock, after the claim |
| `Forward` | `proxy.rs:1527-1530` | An await |
| `Settle` | `HeldClaim::settle`, `proxy.rs:104`; `record`, `proxy_store.rs:413-464` | Under the write lock and the pending lock. A stale token changes nothing and answers `StaleClaim` |
| `SettleFail` | `proxy.rs:123-129` | The failing call commits nothing, so the release is the only commit |
| `Insert` | `insert_or_append_proxy_stub`, `proxy.rs:1148-1156`, called at `:1683` | Under `stubs_write`. Runs after the settle with no await in between |
| `DropRequest` | `HeldClaim::drop`, `proxy.rs:133-139`; `release_claim`, `proxy_store.rs:400-411` | A dropped future or an `Err` return. Only from the awaits: `forward` and `settle` |
| `EditSwap` | `mutate_stubs_from`, `core/mod.rs:845-865`; reconcile, `lifecycle.rs:54-77` | Under `stubs_write` |
| `EditForget` | `forget_orphaned_claims`, `core/mod.rs:907-933` | After the swap, outside the lock, from the swap's own `before` and `after` |
| `ClearStore` | `recording.rs:186`; `clear`, `proxy_store.rs:488-496` | All three locks. Empties `pending` too |
| `ClearSwap`, `ClearForget` | `recording.rs:195-215`, then `core/mod.rs:866` | A `mutate_stubs` like any edit; `overwrite_all` gives the proxy stub a new slot |
| `RestartClear` | `restart_runtime_state`, `manager.rs:3070-3088` | The store is cleared and the stubs stay, carrying their claims |

## What it checks

`ProxyOncePR.cfg` is the gate: two requests, one edit of any kind, a clear, a restart, a store whose
settle can fail, starting recorded. `ProxyOnce.cfg` has three requests, and `ProxyOnceTwoEdits.cfg` two
edits; see "Runs" for why not both.

| Invariant | Meaning | Status |
|:--|:--|:--|
| `NoWedgedClaim` (#1193) | A pending claim has a live holder | holds |
| `ClaimLifetimeIsStubLifetime` (#1386) | A store entry with no carrier exists only inside the settle-to-insert or swap-to-forget window | holds |
| `ShadowedRecordingForwards` (#1705) | A request with the recording behind the proxy stub since it matched does not replay that recording | holds |
| `RecordedOnce` | At most one recording lands between two removals of the entry | holds |
| `LoserNeverRecords` | An `InFlight` request inserts nothing | holds |
| `ClaimExcludesRecording` | A pending claim and a recording never coexist | holds |
| `AtMostOneCarrier` | At most one carrier ahead of the proxy stub | holds with no clear, restart or `deleteProxy`, `ProxyOnceCarriers.cfg` |
| `ForgetRemovesWhatItSaw` | A forget removes only a recording it decided about | holds |
| `RecordingHasItsOwnCarrier` | The entry's own recording has a stub, outside the windows; a carrier of an older one does not count | holds |
| `CarrierHasRecording` | A carrier ahead of the proxy stub has its recording, outside a clear and after a restart | holds with no clear, restart or `deleteProxy`, `ProxyOnceCarriers.cfg` |
| `CarrierMeansRecorded` | A stub carries a claim only if its recording landed | holds |
| `OnlyClaimHolderInserts` | Only a request holding a claim inserts a stub | holds |
| `ForwardUnrecordedOnlyWhenLost` | Only a request that lost the race forwards with no claim | holds |

`witness.py` requires each of these verdicts:

| Case | Expected |
|:--|:--|
| `NeverTwoForwards` | violated: two requests reach the upstream, which the loser path depends on |
| `NeverForgetWhileClaimed` | violated: a forget runs with a claim in flight, which `forget_recorded`'s contract allows |
| `AtMostOneCarrier`, `CarrierHasRecording`, on `ProxyOncePR.cfg` | violated: a clear or a restart, the limits below |
| A proxy that generates no stub (control) | no error |
| `ProxyOncePR.cfg` (control) | no error |

A finding that becomes unreachable means its fix landed: move its invariant to `ProxyOncePR.cfg` and
delete its row.

`mutants.py` puts each bug back, on `ProxyOncePR.cfg`, so the named invariant has to fall first:

| Bug | Caught by |
|:--|:--|
| A dropped request keeps its claim (#1193) | `NoWedgedClaim` |
| An edit does not forget the orphaned claim (#1386) | `ClaimLifetimeIsStubLifetime` |
| No shadow check (#1705) | `ShadowedRecordingForwards` |
| `try_claim` reads `responses` and claims in two steps (#171/#118) | `ClaimExcludesRecording` |
| `record` does not check the token | `RecordedOnce` |
| A failed settle keeps the claim (#315) | `NoWedgedClaim` |
| An empty lookup forwards with no claim and is not lost (#1778) | `ForwardUnrecordedOnlyWhenLost` |
| A stale settle inserts a stub carrying the claim (#1777) | `CarrierMeansRecorded` |
| A failed settle inserts a stub carrying the claim (#1777) | `CarrierMeansRecorded` |
| A forget ignores the token (#1776) | `ForgetRemovesWhatItSaw` |
| Only the first carrier is forgotten (#1776), on `ProxyOnceTwoEdits.cfg` | `ShadowedRecordingForwards` |
| A loser inserts its stub | `LoserNeverRecords` |

The #171/#118 mutant was expected to fall to `RecordedOnce`. A recording lands between the read and the
claim, so the claim on an already recorded signature is the earlier symptom, a step before any second
record.

## Findings

None open. The model found #1776, #1777 and #1778, all fixed. None returned a wrong body or wedged a
signature, and each window was short; the cost was a second upstream call and a duplicate stub. Each has
a test in `crates/rift-mock-core/tests/proxyonce_interleavings.rs` that holds the engine at a probe point to
force the order, and a mutant above. A new finding gets an `#[ignore = "#n"]` test and a `witness.py` row,
and stays out of the cfgs until its fix lands.

### Not filed

- **Replaying through the swap-to-forget window.** A request that matched after an edit removed the
  carrier and before its forget replays the removed recording. This is the "stronger" reading the plan
  asked to confirm at the lookup. It does happen (`the_window_between_an_edit_and_its_forget_replays_the_recording`
  passes), but the window ends with the forget and nothing persists. Recorded as a limit.
- **`ShadowedRecordingForwards` violated, first version of the model.** Refuted: the ghost was wrong. A
  request whose forget was followed by another request's settle replays that newer recording, which is the
  concurrent-first-request behaviour. The invariant now compares against the recording it first saw.
- **Two carriers after the proxy stub is deleted.** With `deleteProxy`, an in-flight request inserts at the
  end of the list, as the doc comment on `insert_or_append_proxy_stub` says, so `AtMostOneCarrier` falls
  with no race in the protocol.
  The second never answers. Refuted as a defect. `ProxyOnceCarriers.cfg` leaves `deleteProxy` out for this reason.
- **A recording that lands before a clear gets its stub after it.** `Settle(r1) -> ClearStore(c1) ->
  ClearSwap(c1) -> Insert(r1)`. The gap has no `.await` in it, and Mountebank's in-flight insert does the
  same. Settling and inserting under `stubs_write` would close it, but holds that lock across a custom
  store's call for a gap of a few instructions, against the reason `forget_orphaned_claims` runs outside it.
- **A restart keeps its stubs.** A request that matched before a stub landed can claim and stack beside it.
  Why `AtMostOneCarrier` falls on `ProxyOncePR.cfg`; `witness.py` keeps it reachable.
- **A store that keeps `forget_recorded_by`'s default** forgets by signature, so it keeps the race #1776
  closed for `LocalProxyStore`, and a delete of an older carrier forgets a newer recording. The model is
  `LocalProxyStore`'s.
- **Two carriers between `ClearStore` and `ClearSwap`.** A request recording in that window stacks a
  carrier, and the swap removes both. Transient, no finding of its own.
- **A carrier with no recording after a restart.** `restart_runtime_state` keeps the stubs on purpose, so
  `CarrierHasRecording` excepts it.

Nothing here is claimed about Mountebank: its source was not consulted for this model.

## Trace validation

`validate.py` asks TLC whether a recorded run is a behaviour of the model (`TraceValidation.tla`). The
points in `crates/rift-mock-core/src/imposter/probe.rs` name no request, so TLC chooses which request
takes each step, and a run is accepted when some choice reaches its end. A step with no point is free: a
`Match`, a shadow check that says no, a drop, every swap. So acceptance says the order is allowed; it does
not pin which edit ran. `ProxyOrphanFound` and `ProxyLookingUp` exist to hold the engine at, and take no
step in the model.

`traces/accept` holds runs recorded from `proxyonce_interleavings.rs`, including the orders the
model predicted when it found its defects. `traces/reject` holds runs written by hand that it must
refuse. A change to the protocol changes these runs, so record them again with
`FORMAL_TRACE_DIR=traces/accept cargo test -p rift-mock-core --all-features --test proxyonce_interleavings -- --include-ignored`.

## Limits

- One signature on one imposter. Different signatures share no store entry; the port's maps are not
  contended beyond their locks.
- `proxyAlways` and `proxyTransparent` are not modelled: they take ungated tokens and never replay
  (`proxy_store.rs:394`).
- A store that publishes stubs is not modelled: it has no local carrier, and `shadowed_recordings` is
  false for it.
- `MAX_TOTAL_SIGNATURES` is not modelled.
- `Unavailable` and `Refused` from `try_claim_as` are not modelled.
- Edits never move a recording back in front of the proxy stub, which would make two carriers by operator
  choice alone.
- Deleting the imposter clears the store from the lifecycle model's side (`formal/lifecycle`); here an
  imposter stop is a `DropRequest`.

## Runs

Measured 2026-11-03 on TLC from the VS Code extension 2026.11.21657, with symmetry on requests and
edits, on a machine shared with other TLC and cargo runs, so the times are rough. Every action has hits
in `ProxyOncePR.cfg` and `ProxyOnce.cfg` under `-coverage 1`.

| Cfg | Bounds | Result | Generated | Distinct | Depth | Time |
|:--|:--|:--|--:|--:|--:|--:|
| `ProxyOncePR.cfg` | 2 requests, 1 edit, clear, restart, failing settle | no error | 112,246 | 40,948 | 24 | 9 s |
| `ProxyOnce.cfg` | 3 requests, 1 edit, clear, restart, failing settle | no error | 2,945,217 | 917,953 | 34 | 56 s |
| `ProxyOnceTwoEdits.cfg` | 2 requests, 2 edits, clear, restart, failing settle | no error | 1,012,092 | 332,258 | 26 | 27 s |

3 requests with 2 edits, a clear, a restart and a failing settle was stopped at 13,029,204 generated and
4,712,697 distinct after 7 minutes, the queue still growing, on a nearly full disk. That combination is
unchecked.

`witness.py` and `mutants.py` exit 0. `validate.py --selftest traces` exits 0: 12 recorded runs accepted,
4 hand-written runs rejected.
