# Formal verification: design record

How Rift checks its concurrent protocols with TLA+, why it does it this way, what that found, and what
it does not cover. It is kept current: a change to the approach updates it. The models themselves are
in `formal/` in the repository, each with a README mapping its actions to the code.

## The problem

Rift's concurrency bugs were found one incident at a time: a deleted imposter still answering (#596,
#1643), a delete that came back on restart (#1124), a stale `304` after a re-create (#1301), a wedged
`proxyOnce` claim (#1193), a claim outliving its stub (#1386), state leaking across a re-create (#1726).

The existing tests could not have found them early. The model-based test
(`crates/rift-mock-core/tests/model/`, #1344) compares the engine with a reference model, but runs one
operation at a time. The differential tests compare with Mountebank, also sequentially. Nothing explored
what happens *between* two steps of two operations, and that is where these bugs live.

## The design

TLC, the TLA+ model checker, explores every interleaving of a small model. Rift has five:

| Model | Protocol | Code |
|:--|:--|:--|
| `formal/lifecycle` | Create, delete, replace and apply racing on a port | `imposter/manager.rs` |
| `formal/proxyonce` | Claiming, recording and forgetting a `proxyOnce` response | `imposter/core/proxy.rs`, `recording/proxy_store.rs` |
| `formal/scenario` | Scenario transitions and their `Last-Modified` stamps | `imposter/core/matching.rs` |
| `formal/scripting` | The JS state cell, worker affinity and live state | `scripting/js_engine.rs` |
| `formal/faultio` | Arming a fault, write order and the lingering close | `imposter/fault_io.rs` |

Each model has the same parts, so that a green run means something:

- **Actions taken from the code.** One action is one step between two points where another task can run.
- **Invariants**, named for what they promise. A model's small cfg gates pull requests, and its larger cfgs run on master and nightly.
- **Witnesses** (`witness.py`). Races a property depends on must stay reachable, or a green run could be vacuous.
- **Mutants** (`mutants.py`). Each fixed bug is put back, and its named invariant must catch it.
- **`Known.cfg`.** Invariants that open defects violate. The gate stays green, and a witness keeps each defect reachable until its fix moves the invariant back.

### From a counterexample to an issue

A TLC counterexample is a lead, not a finding. It says what the *model* allows. So a finding is filed only
after it fails on the real engine, or, where that was not possible, after every step was traced through
the source, and the issue says which.

The tool that makes this practical is the probe (`imposter/probe.rs`): named points the engine calls as
it takes each step. Nothing installs one in production, so the cost there is one `Option` branch. A test
installs one and can **hold the engine at a point**, which turns a race into a deterministic test. Each
defect gets an `#[ignore = "#n"]` test stating the correct behaviour, which fails today. Its fix removes the
attribute.

This rule exists because the first version of the lifecycle model got it wrong. It assumed plain requests
advance the load clock, because a planning note said so. They do not, and an issue was filed with the claim
"confirmed against the source" before anyone had checked. The defect turned out to be real by another route, but
the evidence was not.

Probes also feed trace validation in `lifecycle` and `proxyonce`: a recorded run is written as a `Trace` module, and
TLC searches for a behaviour of the model taking those steps. It checks the model against the engine, the
one direction model checking alone cannot.

### Options weighed

| Option | What it offers | Why not |
|:--|:--|:--|
| loom or shuttle | Explores interleavings of the real code | They replace the synchronisation primitives under test. These races span `.await`s, real sockets, the kernel and hyper. |
| Concurrent operations in the proptest model | Runs real code | Finds races by chance and cannot replay them. It stays useful alongside, and the held tests take its place for known orders. |
| PlusCal | Reads like code | Fits threads blocking on locks. These steps are lock-bracketed and explicit program counters read as well. Hand-written TLA+ throughout. |
| CommunityModules `Json` for traces | Reads ndjson directly | Needs TLC 1.8, which has no immutable artifact. `validate.py` generates a `Trace` module, so one pinned jar is enough. |

## What it found

Fifteen defects, each reproduced by a failing test on the engine. Parts of some were only traced through the
source, and each issue says which. A fixed one says so in the table.

| Issue | Model | Defect |
|:--|:--|:--|
| #1762 | lifecycle | A same-port re-create can answer `304` for new content. Fixed; #1763 closed one route |
| #1763 | lifecycle | A delete clears the shared journal of the imposter that took the port. Fixed |
| #1764 | lifecycle | A create that loses the port claim has already answered requests (observed on macOS only). Fixed |
| #1773 | lifecycle | A failed create's rollback removes the current imposter and leaves its listener running. Fixed |
| #1774 | lifecycle | An apply sweep reports a concurrent delete as failed. Fixed |
| #1776 | proxyonce | A forget removes a newer recording than the one it decided about. Fixed |
| #1777 | proxyonce | A settle that wrote no recording still inserts a stub carrying the claim. Fixed |
| #1778 | proxyonce | An `AlreadyRecorded` with nothing to replay inserts a stub with no claim. Fixed |
| #1779 | scenario | A stalled transition puts a lower `Last-Modified` over a higher one. Fixed |
| #1780 | scenario | A set or delete that decided "no change" can still move the state, unstamped. Fixed |
| #1781 | scenario | A stub scan reads state once per gate, so a transition mid-scan gives a no-match |
| #1782 | faultio | A fault stub added under an open HTTP/2 connection aborts every stream on it |
| #1784 | scripting | A restart between a request's hooks splits it across two state cells. Fixed |
| #1785 | scripting | A getter on the answer runs after the save, so its write is lost. Fixed |
| #1786 | scripting | A job a finished run left fails the run that inherited it. Fixed |

Most need a narrow window, and none was observed without a hold, so how often they occur in production is not
measured.

### Refuted leads

Some suspicions did not survive, and the record is as useful as the findings:

- **A reentrant script run losing updates.** It would, as a mutant shows, but no native a script can call reaches a run entry point.
- **A fault for pipelined response N+1 tripping on N's bytes in the process.** hyper reads the next head only after the buffer flushed. Only the kernel-level form remains, accepted as #1783.
- **The non-atomic default `FlowStore::compare_and_set`.** No shipped path reaches it, so only a third-party store can. It stays a documented limit.
- **`ShadowedRecordingForwards` in `proxyonce`.** Its first violation was a wrong ghost variable in the model.

## Decisions and known limits

- **Defects are filed, not fixed, under this work.** Engine changes here are probe points only.
- **A probe point is where its step commits.** A point that fires and then fails would record a step the engine did not take.
- **A reset discards what the kernel still holds** (#1783, closed). It can take the tail of an earlier pipelined response. That is what a reset means, so it is documented in the fault injection page and kept reachable as a witness.
- **The drain timeout is accepted.** A client too slow to read can be answered after a delete returned; the bound is deliberate.
- **Line numbers in specs rot.** READMEs name functions, and spec comments' lines are as of the commit that wrote them.

Bounds that were not checked, so a pass says nothing about them:

- `proxyonce`: three requests with two edits together. That run did not finish, so the full check is split into two cfgs.
- `scenario`: three gated requests with an admin delete, for the invariants that hold.
- `scripting`: `RunsEnd` across a restart. Safety across restarts is covered, liveness is not.

Not modelled at all: the reload's three separate swaps (imposters, routes, intercept rules), the intercept
listener's install window, `proxyAlways`, publishing proxy stores, HTTPS rebinds, and multi-node behaviour in
rift-cluster. Each model's README lists its own.

## Status

| Model | Small cfg, distinct states | Larger cfgs, distinct states | Mutants |
|:--|--:|--:|--:|
| lifecycle | 131,818 | 299,508 / 715,180 / 1,128,480, liveness 27,212 | 12 |
| proxyonce | 65,543 | 1,871,961 / 854,867 | 7 |
| scenario | 3,075,039 | 3,185,556 | 3 |
| scripting | 760,890 | 18,563,719, liveness 192,992 | 8 |
| faultio | 42,894 | 721,960 | 6 |

Not yet done: model-based replay, which runs behaviours generated from a model against a pure reference such as
`tests/model/model.rs`. Trace validation for `scenario`, `scripting` and `faultio` is also not done.

## Changing it

- **Changing modelled code.** Change the model too, or put a line starting `formal:` in the pull request's description saying why it is unaffected. `.github/workflows/formal.yml` enforces this.
- **Fixing a defect.** Remove the test's `#[ignore]`, move the invariant from `Known.cfg` back into the cfgs, delete its witness row, and add a mutant that puts the bug back.
- **A new counterexample.** Reproduce it before filing it.
- **Changing the approach.** Update this page.
