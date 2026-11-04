----------------------------- MODULE Lifecycle -----------------------------
(***************************************************************************)
(* The imposter generation lifecycle of `ImposterManager`                  *)
(* (crates/rift-mock-core/src/imposter/manager.rs): how a create, a       *)
(* delete, a replace, an `apply_config` sweep and a `delete_all` move an   *)
(* imposter ("generation") through the port table, the datadir file, the   *)
(* load floor and the port-keyed shared state, while requests are served.  *)
(*                                                                         *)
(* Every action is one step the code takes between two points where       *)
(* another task can run: an `.await`, a lock release, or a lock-free load. *)
(* Steps under one lock (`PortTable.mutations`, `persist_lock`) are one    *)
(* action.  Each action's comment names the line it models; README.md has  *)
(* the table.  The wall clock never advances, which is the worst case the  *)
(* floor exists for (issue #1301): every LoadClock::next lands in the same *)
(* second.  A plain request reads the stamp its content already has; only  *)
(* a content change (a stub write, a gated scenario transition) advances   *)
(* the clock (core/mod.rs:155).                                            *)
(*                                                                         *)
(* A generation is <<port, n>>: the n-th imposter created on that port.    *)
(* Ghost flags record a property violation at the step that commits it,   *)
(* so an invariant is one flag and TLC's trace ends on the guilty step.    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Ports,          \* the ports imposters are created on
    Creators,       \* ops that each run `create_imposter` once (manager.rs:1352)
    Deleters,       \* ops that each run `delete_imposter` once (manager.rs:2107)
    Appliers,       \* ops that each run `apply_config` once toward `ApplyDesired` (manager.rs:2377)
    ApplyOnes,      \* ops that each run `apply_one` once on some port (manager.rs:2448)
    DeleteAlls,     \* ops that each run `delete_all` once (manager.rs:2222)
    ApplyDesired,   \* the ports an `apply_config` keeps (its desired set); every other port is swept
    MaxGens,        \* generations per port, counting a preloaded one
    MaxReqs,        \* requests served per generation
    MaxChanges,     \* content changes per generation (stub writes, gated scenario transitions)
    Preloaded,      \* TRUE: every port starts with a live, persisted generation
    PersistFails,   \* TRUE: a create's datadir write may fail (manager.rs:1560)
    UnlinkFails,    \* TRUE: a delete's unlink may fail (manager.rs:2122)
    DrainTimesOut,  \* TRUE: a delete's connection drain may time out (manager.rs:2167)
    NoGen,          \* the empty slot / file / shared-state value
    NoOp            \* nobody holds `persist_lock`

Ops == Creators \cup Deleters \cup Appliers \cup ApplyOnes \cup DeleteAlls

ASSUME /\ Ports # {}
       /\ MaxGens >= 1 /\ MaxReqs >= 1
       /\ ApplyDesired \subseteq Ports
       /\ \A s \in {Creators, Deleters, Appliers, ApplyOnes, DeleteAlls} :
            \A t \in {Creators, Deleters, Appliers, ApplyOnes, DeleteAlls} :
              s # t => s \cap t = {}

GenIds == Ports \X (1..MaxGens)
Gen(p, n) == <<p, n>>
PortOf(g) == g[1]
Max(a, b) == IF a >= b THEN a ELSE b
Min(S) == CHOOSE q \in S : \A r \in S : q <= r

\* A generation answers requests once it holds the port, has re-read the floor, and its cancel token
\* is untouched.  "accepting" and "admitting" generations' loops wait behind a closed gate (#1764).
Serving == {"claimed"}

VARIABLES
    slot,           \* [Ports -> GenIds \cup {NoGen}]: `PortTable` (manager.rs:476)
    file,           \* [Ports -> GenIds \cup {NoGen}]: the datadir `{port}.json`, by the generation that wrote it
    floor,          \* [Ports -> Nat]: `load_floors` (manager.rs:2095); 0 = none recorded
    nextGen,        \* [Ports -> Nat]: the next generation number per port
    persistLock,    \* Ops \cup {NoOp}: `persist_lock` (a tokio Mutex)
    phase,          \* [GenIds -> {"unborn", "accepting", "admitting", "claimed", "cancelled", "dead"}]
    clock,          \* [GenIds -> [last, stamp, changes]]: `last` is the generation's `LoadClock`
                    \*   (core/mod.rs:155), `stamp` the stamp its content is served under
    nreq,           \* [GenIds -> Nat]: requests this generation has started (a bound)
    req,            \* [GenIds -> {"none", "computing", "writing"}]: its in-flight request
    deleteReturned, \* [GenIds -> BOOLEAN]: a delete of it has returned
    lastStamp,      \* [Ports -> [s, g]]: the highest `Last-Modified` the port's registered
                    \*   generations served, and by whom
    shared,         \* [Ports -> GenIds \cup {NoGen}]: port-keyed shared state (journal, sequencer scope,
                    \*   shared proxy store), by the generation that last wrote it
    op,             \* [Ops -> record]: each operation's program counter and locals
    ghost           \* property flags, see Invariants

vars == <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
          deleteReturned, lastStamp, shared, op, ghost>>

(***************************************************************************)
(* Operations.  `pc` is where the op is; `ret` is where its current create *)
(* or delete sub-path returns to; `port`/`gen`/`look` are its locals;      *)
(* `plan` the ports an apply/delete_all sweep still deletes, `todo` the    *)
(* desired ports an apply still reconciles; `unlink` whether its delete    *)
(* removes `{port}.json` (a replace's does not, manager.rs:2654); `fail`   *)
(* how its last create ended; `ok` whether its last delete succeeded.      *)
(***************************************************************************)
KindOf(o) == CASE o \in Creators   -> "create"
               [] o \in Deleters   -> "delete"
               [] o \in Appliers   -> "apply"
               [] o \in ApplyOnes  -> "applyOne"
               [] o \in DeleteAlls -> "deleteAll"

InitPc(o) == CASE KindOf(o) = "create"    -> "cBind"
               [] KindOf(o) = "delete"    -> "dLock"
               [] KindOf(o) = "apply"     -> "aSnap"
               [] KindOf(o) = "applyOne"  -> "aEntry"
               [] KindOf(o) = "deleteAll" -> "aSnap"

InitOp(o, p) == [kind |-> KindOf(o), pc |-> InitPc(o), ret |-> "done", port |-> p,
                 gen |-> NoGen, look |-> NoGen,
                 plan |-> {}, todo |-> IF KindOf(o) = "applyOne" THEN {p} ELSE {},
                 unlink |-> TRUE, fail |-> "none", ok |-> FALSE]

Quiescent == /\ \A o \in Ops : op[o].pc = "done"
             /\ \A g \in GenIds : req[g] = "none"

NoGhost == [stale |-> FALSE, wiped |-> FALSE, lateAnswer |-> FALSE, removedOther |-> FALSE,
            lostClaim |-> FALSE, phantomFail |-> FALSE, errDeleteGone |-> FALSE,
            loserAnswered |-> FALSE]

\* The frozen wall clock: Imposter::new stamps its content with LoadClock::next, which is "now"
\* on a fresh clock (core/mod.rs:680-681).
Now == 1
Unstamped == [last |-> Now, stamp |-> Now, changes |-> 0]

Init ==
    /\ slot = [p \in Ports |-> IF Preloaded THEN Gen(p, 1) ELSE NoGen]
    /\ file = slot
    /\ floor = [p \in Ports |-> 0]
    /\ nextGen = [p \in Ports |-> IF Preloaded THEN 2 ELSE 1]
    /\ persistLock = NoOp
    /\ phase = [g \in GenIds |-> IF Preloaded /\ g[2] = 1 THEN "claimed" ELSE "unborn"]
    /\ clock = [g \in GenIds |-> Unstamped]
    /\ nreq = [g \in GenIds |-> 0]
    /\ req = [g \in GenIds |-> "none"]
    /\ deleteReturned = [g \in GenIds |-> FALSE]
    /\ lastStamp = [p \in Ports |-> [s |-> 0, g |-> NoGen]]
    /\ shared = [p \in Ports |-> NoGen]
    /\ \E f \in [Ops -> Ports] : op = [o \in Ops |-> InitOp(o, f[o])]
    /\ ghost = NoGhost

\* Cut the request a cancel catches while it is still computing (handler.rs:708, issue #1643).
Cut(r) == IF r = "computing" THEN "none" ELSE r

(***************************************************************************)
(* Create: create_imposter_staged (manager.rs:1352).  Entered at "cBind"   *)
(* by a creator, by apply_entry for an absent port (manager.rs:2489) and   *)
(* by replace_imposter after its delete (manager.rs:2660).                 *)
(***************************************************************************)

\* bind_for_create's `contains` check (manager.rs:1588): PortInUse when the slot is held.  A creator
\* picks its port here; an apply's create has it already.
CreateBindBusy(o) ==
    /\ op[o].pc = "cBind"
    /\ slot[op[o].port] # NoGen
    /\ op' = [op EXCEPT ![o].pc = op[o].ret, ![o].fail = "portInUse", ![o].gen = NoGen]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* The model's bound on generations, not a step of the code: a create the bound refuses returns
\* as if it failed, so no op stalls on the bound.
CreateBindBound(o) ==
    /\ op[o].pc = "cBind"
    /\ slot[op[o].port] = NoGen
    /\ nextGen[op[o].port] > MaxGens
    /\ op' = [op EXCEPT ![o].pc = op[o].ret, ![o].fail = "bound", ![o].gen = NoGen]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* The slot is free: bind (SO_REUSEADDR/REUSEPORT, so a concurrent create binds too) and build
\* the imposter (manager.rs:1588-1470).  The generation is numbered here.
CreateBindOk(o) ==
    /\ op[o].pc = "cBind"
    /\ slot[op[o].port] = NoGen
    /\ nextGen[op[o].port] <= MaxGens
    /\ LET p == op[o].port
           g == Gen(p, nextGen[p])
       IN /\ nextGen' = [nextGen EXCEPT ![p] = @ + 1]
          /\ op' = [op EXCEPT ![o].pc = "cFloor", ![o].gen = g, ![o].fail = "none"]
    /\ UNCHANGED <<slot, file, floor, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* Read the port's load floor into the new generation's clock (manager.rs:1477,
\* `continue_load_clock`, core/mod.rs:962): raise the clock, and restamp the content unless it is
\* already above the floor.  Before the claim, and before any delete of the current holder records
\* its floor.
CreateReadFloor(o) ==
    /\ op[o].pc = "cFloor"
    /\ LET g == op[o].gen
           f == floor[op[o].port]
           raised == Max(clock[g].last, f)
       IN clock' = [clock EXCEPT ![g] =
              IF @.stamp > f THEN [@ EXCEPT !.last = raised]
                             ELSE [@ EXCEPT !.last = raised + 1, !.stamp = raised + 1]]
    /\ op' = [op EXCEPT ![o].pc = "cAccept"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* spawn_accept_loops: the loops run, but wait for the claim before their first accept (#1764).
CreateSpawnAccept(o) ==
    /\ op[o].pc = "cAccept"
    /\ phase' = [phase EXCEPT ![op[o].gen] = "accepting"]
    /\ op' = [op EXCEPT ![o].pc = "cClaim"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* PortTable::try_claim: check and store under `mutations`.  The loser cancels and stops its
\* listener and returns PortInUse.
CreateTryClaim(o) ==
    /\ op[o].pc = "cClaim"
    /\ LET p == op[o].port
           g == op[o].gen
       IN IF slot[p] = NoGen
            THEN /\ slot' = [slot EXCEPT ![p] = g]
                 /\ phase' = [phase EXCEPT ![g] = "admitting"]
                 /\ op' = [op EXCEPT ![o].pc = "cReread"]
                 /\ UNCHANGED <<req, ghost>>
            ELSE /\ phase' = [phase EXCEPT ![g] = "cancelled"]
                 /\ req' = [req EXCEPT ![g] = Cut(@)]
                 /\ ghost' = [ghost EXCEPT !.loserAnswered = @ \/ nreq[g] > 0]
                 /\ op' = [op EXCEPT ![o].pc = op[o].ret, ![o].fail = "portInUse"]
                 /\ UNCHANGED slot
    /\ UNCHANGED <<file, floor, nextGen, persistLock, clock, nreq, deleteReturned,
                   lastStamp, shared>>

\* Reachable only in the `no mutations lock` mutant (mutants.py), which splits CreateTryClaim
\* into a check and this store.
CreateClaimStore(o) ==
    /\ op[o].pc = "cStore"
    /\ LET p == op[o].port
           g == op[o].gen
       IN /\ ghost' = [ghost EXCEPT !.lostClaim = @ \/ slot[p] # NoGen]
          /\ slot' = [slot EXCEPT ![p] = g]
          /\ phase' = [phase EXCEPT ![g] = "admitting"]
          /\ op' = [op EXCEPT ![o].pc = "cReread"]
    /\ UNCHANGED <<file, floor, nextGen, persistLock, clock, nreq, req, deleteReturned,
                   lastStamp, shared>>

\* After the claim: re-read the floor, which a delete may have recorded since CreateReadFloor, then
\* open the gate (#1762).  A delete may already have cancelled this generation, which stays cancelled.
CreateReread(o) ==
    /\ op[o].pc = "cReread"
    /\ LET g == op[o].gen
           again == floor[op[o].port]
           raised == Max(clock[g].last, again)
       IN /\ clock' = [clock EXCEPT ![g] =
                 IF @.stamp > again THEN [@ EXCEPT !.last = raised]
                                    ELSE [@ EXCEPT !.last = raised + 1, !.stamp = raised + 1]]
          /\ phase' = IF phase[g] = "admitting" THEN [phase EXCEPT ![g] = "claimed"] ELSE phase
    /\ op' = [op EXCEPT ![o].pc = "cPersist"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, nreq, req, deleteReturned, lastStamp,
                   shared, ghost>>

\* persist_imposter_with takes `persist_lock` (manager.rs:3080).
CreatePersistLock(o) ==
    /\ op[o].pc = "cPersist"
    /\ persistLock = NoOp
    /\ persistLock' = o
    /\ op' = [op EXCEPT ![o].pc = "cWrite"]
    /\ UNCHANGED <<slot, file, floor, nextGen, phase, clock, nreq, req, deleteReturned,
                   lastStamp, shared, ghost>>

\* Under the lock: a non-current imposter is not written (manager.rs:3083, issue #1124); else the
\* file is replaced (manager.rs:3113), or the write fails and the create rolls back.
CreatePersistWrite(o) ==
    /\ op[o].pc = "cWrite"
    /\ persistLock' = NoOp
    /\ LET p == op[o].port
           g == op[o].gen
       IN IF slot[p] # g
            THEN /\ op' = [op EXCEPT ![o].pc = op[o].ret, ![o].fail = "none"]
                 /\ UNCHANGED file
            ELSE \/ /\ file' = [file EXCEPT ![p] = g]
                    /\ op' = [op EXCEPT ![o].pc = op[o].ret, ![o].fail = "none"]
                 \/ /\ PersistFails
                    /\ op' = [op EXCEPT ![o].pc = "cRollback"]
                    /\ UNCHANGED file
    /\ UNCHANGED <<slot, floor, nextGen, phase, clock, nreq, req, deleteReturned,
                   lastStamp, shared, ghost>>

\* The rollback of a failed persist (create_imposter_staged): `remove_if_same` under `persist_lock`,
\* then cancel.  The slot's holder may no longer be this generation (issue #1773).
CreateRollback(o) ==
    /\ op[o].pc = "cRollback"
    /\ persistLock = NoOp
    /\ LET p == op[o].port
           g == op[o].gen
       IN /\ ghost' = ghost
          /\ slot' = IF slot[p] = g THEN [slot EXCEPT ![p] = NoGen] ELSE slot
          /\ phase' = [phase EXCEPT ![g] = "cancelled"]
          /\ floor' = [floor EXCEPT ![p] = Max(@, clock[g].last)]
          /\ req' = [req EXCEPT ![g] = Cut(@)]
          /\ op' = [op EXCEPT ![o].pc = op[o].ret, ![o].fail = "persist"]
    /\ UNCHANGED <<file, nextGen, persistLock, clock, nreq, deleteReturned,
                   lastStamp, shared>>

(***************************************************************************)
(* Delete: delete_imposter_inner (manager.rs:2107).  Entered at "dLock" by *)
(* a deleter and by an apply's or delete_all's sweep (unlink = TRUE), and  *)
(* at "rLock" by replace_imposter (unlink = FALSE).                        *)
(***************************************************************************)

\* Take `persist_lock` (manager.rs:2117).
DeleteLock(o) ==
    /\ op[o].pc = "dLock"
    /\ persistLock = NoOp
    /\ persistLock' = o
    /\ op' = [op EXCEPT ![o].pc = "dLookup"]
    /\ UNCHANGED <<slot, file, floor, nextGen, phase, clock, nreq, req, deleteReturned,
                   lastStamp, shared, ghost>>

\* get_imposter (manager.rs:2118): NotFound releases the lock and returns.
DeleteLookup(o) ==
    /\ op[o].pc = "dLookup"
    /\ LET p == op[o].port
       IN IF slot[p] = NoGen
            THEN /\ persistLock' = NoOp
                 /\ op' = [op EXCEPT ![o].pc = "dNotFound"]
            ELSE /\ op' = [op EXCEPT ![o].pc = "dUnlink", ![o].look = slot[p]]
                 /\ UNCHANGED persistLock
    /\ UNCHANGED <<slot, file, floor, nextGen, phase, clock, nreq, req, deleteReturned,
                   lastStamp, shared, ghost>>

\* remove_persisted_imposter: the file goes first (issue #1124).  A failed unlink leaves the
\* imposter registered and serving, and `Err` means "not deleted".
DeleteUnlink(o) ==
    /\ op[o].pc = "dUnlink"
    /\ LET p == op[o].port
           g == op[o].look
       IN \/ /\ file' = [file EXCEPT ![p] = NoGen]
             /\ op' = [op EXCEPT ![o].pc = "dCancel"]
             /\ UNCHANGED <<slot, persistLock, ghost>>
          \/ /\ UnlinkFails
             /\ persistLock' = NoOp
             /\ UNCHANGED <<slot, file>>
             /\ ghost' = [ghost EXCEPT !.errDeleteGone =
                    @ \/ slot'[p] # g \/ phase[g] \notin Serving \cup {"admitting"}]
             /\ op' = [op EXCEPT ![o].pc = "dErr"]
    /\ UNCHANGED <<floor, nextGen, phase, clock, nreq, req, deleteReturned, lastStamp, shared>>

\* A replace's delete takes `persist_lock` too, and unlinks nothing: its create overwrites the file.
ReplaceLock(o) ==
    /\ op[o].pc = "rLock"
    /\ persistLock = NoOp
    /\ persistLock' = o
    /\ op' = [op EXCEPT ![o].pc = "rLookup"]
    /\ UNCHANGED <<slot, file, floor, nextGen, phase, clock, nreq, req, deleteReturned,
                   lastStamp, shared, ghost>>

ReplaceLookup(o) ==
    /\ op[o].pc = "rLookup"
    /\ LET p == op[o].port
       IN IF slot[p] = NoGen
            THEN /\ persistLock' = NoOp
                 /\ op' = [op EXCEPT ![o].pc = "dNotFound"]
            ELSE /\ op' = [op EXCEPT ![o].pc = "dCancel", ![o].look = slot[p]]
                 /\ UNCHANGED persistLock
    /\ UNCHANGED <<slot, file, floor, nextGen, phase, clock, nreq, req, deleteReturned,
                   lastStamp, shared, ghost>>

\* cancel.cancel() and the shutdown signal: every request still computing is cut off (issue
\* #1643); one already writing finishes under hyper's graceful shutdown.
DeleteCancel(o) ==
    /\ op[o].pc = "dCancel"
    /\ LET g == op[o].look
       IN /\ phase' = [phase EXCEPT ![g] = "cancelled"]
          /\ req' = [req EXCEPT ![g] = Cut(@)]
    /\ op' = [op EXCEPT ![o].pc = "dClear"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, clock, nreq, deleteReturned,
                   lastStamp, shared, ghost>>

\* clear_shared_state: sequencer scope, journal, shared proxy store, keyed by port, while the slot
\* still holds this generation (issue #1763).
DeleteClearShared(o) ==
    /\ op[o].pc = "dClear"
    /\ LET p == op[o].port
           g == op[o].look
       IN /\ ghost' = [ghost EXCEPT !.wiped =
                 @ \/ (shared[p] # NoGen /\ shared[p] # g /\ shared[p] = slot[p])]
          /\ shared' = [shared EXCEPT ![p] = NoGen]
          /\ op' = [op EXCEPT ![o].pc = "dFloor"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp>>

\* record_load_floor: keep the generation's clock for the next one.  After the cancel, so no stamp
\* can land above it (issue #1762).
DeleteRecordFloor(o) ==
    /\ op[o].pc = "dFloor"
    /\ floor' = [floor EXCEPT ![op[o].port] = Max(@, clock[op[o].look].last)]
    /\ op' = [op EXCEPT ![o].pc = "dRemove"]
    /\ UNCHANGED <<slot, file, nextGen, persistLock, phase, clock, nreq, req, deleteReturned,
                   lastStamp, shared, ghost>>

\* PortTable::remove_if_same: vacate the slot only if it still holds the looked-up generation.
\* The lock is released when the block ends.
DeleteRemoveIfSame(o) ==
    /\ op[o].pc = "dRemove"
    /\ persistLock' = NoOp
    /\ LET p == op[o].port
       IN IF slot[p] = op[o].look
            THEN /\ ghost' = [ghost EXCEPT !.removedOther = @ \/ slot[p] # op[o].look]
                 /\ slot' = [slot EXCEPT ![p] = NoGen]
                 /\ op' = [op EXCEPT ![o].pc = "dDrain"]
            ELSE /\ op' = [op EXCEPT ![o].pc = "dNotFound"]
                 /\ UNCHANGED <<slot, ghost>>
    /\ UNCHANGED <<file, floor, nextGen, phase, clock, nreq, req, deleteReturned,
                   lastStamp, shared>>

\* Await the accept loops, then drain the connections within `conn_drain`.  A response still being
\* written is waited for, or, when the bound elapses, left to finish after the delete returns.
DeleteDrain(o) ==
    /\ op[o].pc = "dDrain"
    /\ req[op[o].look] # "writing" \/ DrainTimesOut
    /\ op' = [op EXCEPT ![o].pc = "dFinish"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

DeleteFinish(o) ==
    /\ op[o].pc = "dFinish"
    /\ LET g == op[o].look
       IN /\ phase' = [phase EXCEPT ![g] = "dead"]
          /\ deleteReturned' = [deleteReturned EXCEPT ![g] = TRUE]
          /\ op' = [op EXCEPT ![o].pc = "dDone"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, clock, nreq, req, lastStamp, shared,
                   ghost>>

\* The delete's three outcomes return to the op.  An apply reports NotFound as a failure
\* (manager.rs:2401, 2655); delete_all tolerates it (manager.rs:2228); a deleter just returns it.
DeleteReturn(o) ==
    /\ op[o].pc \in {"dDone", "dNotFound", "dErr"}
    /\ ghost' = [ghost EXCEPT !.phantomFail =
          @ \/ (op[o].pc = "dNotFound" /\ op[o].kind \in {"apply", "applyOne"}
                  /\ op[o].ret # "rCreate")]
    /\ op' = [op EXCEPT ![o].pc = op[o].ret, ![o].ok = (op[o].pc = "dDone")]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared>>

(***************************************************************************)
(* apply_config (manager.rs:2377) and delete_all (manager.rs:2222): a      *)
(* `ports()` snapshot, a delete per swept port, then one entry per desired *)
(* port.  apply_one (manager.rs:2448) is the entry alone.                  *)
(***************************************************************************)

\* The `ports()` snapshot (manager.rs:2392, 2224).
ApplySnapshot(o) ==
    /\ op[o].pc = "aSnap"
    /\ LET keep == IF op[o].kind = "apply" THEN ApplyDesired ELSE {}
       IN op' = [op EXCEPT ![o].pc = "aSweep",
                           ![o].plan = {q \in Ports : slot[q] # NoGen /\ q \notin keep},
                           ![o].todo = keep]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* Delete the next swept port, ascending (manager.rs:2398).
ApplySweep(o) ==
    /\ op[o].pc = "aSweep"
    /\ IF op[o].plan = {}
         THEN op' = [op EXCEPT ![o].pc = "aEntry"]
         ELSE LET q == Min(op[o].plan)
              IN op' = [op EXCEPT ![o].pc = "dLock", ![o].ret = "aSweep", ![o].port = q,
                                  ![o].plan = @ \ {q}, ![o].unlink = TRUE]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* apply_entry (manager.rs:2489): get_imposter decides.  Absent: create_for_apply.  Present: the
\* config is unchanged or patched in place (no lifecycle step), or differs at the imposter level
\* and the imposter is replaced (manager.rs:2643).
ApplyEntry(o) ==
    /\ op[o].pc = "aEntry"
    /\ IF op[o].todo = {}
         THEN op' = [op EXCEPT ![o].pc = "done"]
         ELSE LET q == Min(op[o].todo)
              IN IF slot[q] = NoGen
                   THEN op' = [op EXCEPT ![o].pc = "cBind", ![o].ret = "aEntry", ![o].port = q,
                                         ![o].todo = @ \ {q}]
                   ELSE \/ op' = [op EXCEPT ![o].todo = @ \ {q}]
                        \/ op' = [op EXCEPT ![o].pc = "rLock", ![o].ret = "rCreate",
                                            ![o].port = q, ![o].todo = @ \ {q},
                                            ![o].unlink = FALSE]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* replace_imposter after its delete: create on the port.  A delete that found the port empty lost
\* to a concurrent one, and a vacant port is what it was for.
ReplaceAfterDelete(o) ==
    /\ op[o].pc = "rCreate"
    /\ op' = [op EXCEPT ![o].pc = "cBind", ![o].ret = "rAfter"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

\* replace_imposter after its create (manager.rs:2672-2686): a create that failed for a reason
\* other than PortInUse removes the port's file if the slot is vacant, so a restart does not
\* bring the replaced imposter back.
ReplaceAfterCreate(o) ==
    /\ op[o].pc = "rAfter"
    /\ LET p == op[o].port
       IN file' = IF op[o].fail = "persist" /\ slot[p] = NoGen
                    THEN [file EXCEPT ![p] = NoGen] ELSE file
    /\ op' = [op EXCEPT ![o].pc = "aEntry"]
    /\ UNCHANGED <<slot, floor, nextGen, persistLock, phase, clock, nreq, req,
                   deleteReturned, lastStamp, shared, ghost>>

(***************************************************************************)
(* Requests, per generation: accepted by its listener, computed (the       *)
(* response and its Last-Modified stamp, handler.rs:2065; the journal and  *)
(* recordings written), then written to the client.                        *)
(***************************************************************************)

ReqStart(g) ==
    /\ phase[g] \in Serving
    /\ req[g] = "none"
    /\ nreq[g] < MaxReqs
    /\ req' = [req EXCEPT ![g] = "computing"]
    /\ nreq' = [nreq EXCEPT ![g] = @ + 1]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, deleteReturned,
                   lastStamp, shared, op, ghost>>

\* The response carries the stamp its content already has (served_load_stamp,
\* core/matching.rs:592); no clock moves.  A later generation serving at or below a stamp an earlier
\* one served lets a client's `If-Modified-Since` keep a stale answer (issue #1301).  Only the
\* registered generation's stamps count: a loser of the claim race answering is LoserNeverAnswers'
\* concern, and folding its stamps in would hide the floor race behind that shorter trace.  The
\* request's journal entry and any recording land in the port's shared state whoever serves it.
ReqCompute(g) ==
    /\ phase[g] \in Serving
    /\ req[g] = "computing"
    /\ LET p == PortOf(g)
           s == clock[g].stamp
           registered == slot[p] = g
       IN /\ ghost' = [ghost EXCEPT !.stale =
                 @ \/ (registered /\ lastStamp[p].g \notin {NoGen, g} /\ s <= lastStamp[p].s)]
          /\ lastStamp' = [lastStamp EXCEPT ![p] =
                 IF registered THEN [s |-> Max(@.s, s), g |-> g] ELSE @]
          /\ shared' = [shared EXCEPT ![p] = g]
          /\ req' = [req EXCEPT ![g] = "writing"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, deleteReturned, op>>

\* A stub write or a gated scenario transition on a conditional stub (stamp_scenario_transition,
\* core/matching.rs:568): the one step that advances a generation's clock.  It needs no lock a
\* delete holds, so it can land during a delete, until the cancel.
ContentChange(g) ==
    /\ phase[g] \in Serving
    /\ clock[g].changes < MaxChanges
    /\ clock' = [clock EXCEPT ![g] = [last |-> @.last + 1, stamp |-> @.last + 1,
                                      changes |-> @.changes + 1]]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, nreq, req, deleteReturned,
                   lastStamp, shared, op, ghost>>

\* The response reaches the client.  After the delete of its generation returned, that is a byte
\* served by a deleted imposter (issue #596).
ReqWrite(g) ==
    /\ req[g] = "writing"
    /\ ghost' = [ghost EXCEPT !.lateAnswer = @ \/ deleteReturned[g]]
    /\ req' = [req EXCEPT ![g] = "none"]
    /\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, deleteReturned,
                   lastStamp, shared, op>>

(***************************************************************************)
(* The specification.                                                      *)
(***************************************************************************)

OpStep(o) ==
    \/ CreateBindBusy(o) \/ CreateBindBound(o) \/ CreateBindOk(o) \/ CreateReadFloor(o)
    \/ CreateSpawnAccept(o)
    \/ CreateTryClaim(o) \/ CreateClaimStore(o) \/ CreateReread(o) \/ CreatePersistLock(o) \/ CreatePersistWrite(o)
    \/ CreateRollback(o)
    \/ DeleteLock(o) \/ DeleteLookup(o) \/ DeleteUnlink(o) \/ DeleteRecordFloor(o)
    \/ DeleteRemoveIfSame(o) \/ ReplaceLock(o) \/ ReplaceLookup(o) \/ DeleteCancel(o)
    \/ DeleteDrain(o) \/ DeleteClearShared(o) \/ DeleteFinish(o) \/ DeleteReturn(o)
    \/ ApplySnapshot(o) \/ ApplySweep(o) \/ ApplyEntry(o) \/ ReplaceAfterDelete(o)
    \/ ReplaceAfterCreate(o)

ReqStep(g) == ReqStart(g) \/ ReqCompute(g) \/ ReqWrite(g)

\* Every op has finished and no request is in flight: the model stutters, so TLC's deadlock check
\* reports only a state where some op can never finish.
Terminating == Quiescent /\ UNCHANGED vars

Next ==
    \/ \E o \in Ops : OpStep(o)
    \/ \E g \in GenIds : ReqStep(g) \/ ContentChange(g)
    \/ Terminating

Spec == Init /\ [][Next]_vars

\* Every step the engine's own tasks take is eventually taken; requests are eventually written.
FairSpec == Spec /\ \A o \in Ops : WF_vars(OpStep(o)) /\ \A g \in GenIds : WF_vars(ReqStep(g))

(***************************************************************************)
(* Invariants.                                                             *)
(***************************************************************************)

TypeInv ==
    /\ slot \in [Ports -> GenIds \cup {NoGen}]
    /\ file \in [Ports -> GenIds \cup {NoGen}]
    /\ floor \in [Ports -> Nat]
    /\ persistLock \in Ops \cup {NoOp}
    /\ phase \in [GenIds -> {"unborn", "accepting", "admitting", "claimed", "cancelled", "dead"}]
    /\ req \in [GenIds -> {"none", "computing", "writing"}]
    /\ \A g \in GenIds : nreq[g] <= MaxReqs /\ clock[g].changes <= MaxChanges

\* No byte is served by a generation after the delete of it returned (issues #596, #1643).
NoServeAfterDeleteReturns == ~ghost.lateAnswer

\* A port's Last-Modified stamps rise across its generations: a new generation never serves at or
\* below the stamp the previous one last served (issue #1301).
FloorMonotone == ~ghost.stale

\* A delete, a replace or a rollback only ever vacates the slot of the generation it looked up
\* (issue #1124).
RemoveOnlyOwn == ~ghost.removedOther

\* `Err` from a delete means the imposter is still registered and serving.
ErrDeleteStillServing == ~ghost.errDeleteGone

\* `Err` from a create means the imposter is not running: not registered, its listener stopped
\* (issue #563).  Stable once the create returned: no later step registers that generation.
ErrCreateNotRunning ==
    \A o \in Creators :
        (op[o].pc = "done" /\ op[o].fail # "none" /\ op[o].gen # NoGen)
          => slot[op[o].port] # op[o].gen /\ phase[op[o].gen] \in {"cancelled", "dead"}

\* A create that failed with PortInUse never answered a request: the loser of the claim race had
\* no listener a client could reach.
LoserNeverAnswers == ~ghost.loserAnswered

\* Only one generation ever claims a slot (PortTable::try_claim under `mutations`).
OneClaimant == ~ghost.lostClaim

\* The clear a delete runs after vacating the slot never wipes the shared state the port's
\* current generation wrote.
SharedStateBelongsToCurrent == ~ghost.wiped

\* An apply never reports as failed a port that is already in the state it asked for.
ApplyReportsTruth == ~ghost.phantomFail

\* When nothing runs, the datadir and the table name the same generation on every port.
TableFileAgree == Quiescent => \A p \in Ports : file[p] = slot[p]

\* Every slot holds a generation that claimed it: serving, or being deleted under `persist_lock`.
TableHoldsLive == \A p \in Ports : slot[p] # NoGen => phase[slot[p]] \in {"admitting", "claimed", "cancelled"}

\* Witness (witness.py): PortInUse comes only from the bind check, never from the claim.  Violated
\* by design: two creates pass the check and both bind under SO_REUSEPORT (manager.rs:1540), and
\* the claim decides.
BindCheckSuffices ==
    \A o \in Creators : op[o].pc = "done" /\ op[o].fail = "portInUse" => op[o].gen = NoGen

\* Under FairSpec: every operation returns.
OpsFinish == <>[]Quiescent

\* Creators are interchangeable, and so are deleters.
Symmetry == Permutations(Creators) \cup Permutations(Deleters)

=============================================================================
