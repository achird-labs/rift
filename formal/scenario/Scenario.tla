----------------------------- MODULE Scenario -----------------------------
(***************************************************************************)
(* Scenario transitions and the `Last-Modified` stamps they issue, on one  *)
(* imposter, one flow and one scenario (`core/matching.rs`, `handler.rs`,  *)
(* `extensions/flow_state.rs`).                                            *)
(*                                                                         *)
(* Every action is one step the code takes between two points where       *)
(* another task can run.  Nothing here holds a lock across steps: the flow *)
(* store takes its lock per call (`backends/inmemory.rs:294`), the load    *)
(* clock is one atomic update (`core/mod.rs:169`) and the stamp map one    *)
(* `ArcSwap::rcu` (`core/matching.rs:580`), and the worker threads run in  *)
(* parallel, so any step of one request can land between two of another.  *)
(* README.md has the table of actions and lines.                           *)
(*                                                                         *)
(* The stub table is a cycle: `Started -> A -> B -> Started`.  A request   *)
(* in `Gated` matches the gated stubs that carry the transition; one in    *)
(* `Pollers` matches read-only stubs with the same gates, in the same      *)
(* order; a request's predicates decide which, and never change.  Every    *)
(* state has a stub, so a gated request always has an answer to give.      *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Gated,      \* requests matching the gated stubs, each with a `newScenarioState`
    Pollers,    \* requests matching read-only stubs under the same gates
    Setters,    \* requests matching an ungated stub with `newScenarioState = SetTo`
    Resetters,  \* admin deletes of the scenario's key (`flow_delete`, matching.rs:746)
    Start,      \* what the store holds at the start: a state, or NoVal (absent)
    SetTo,      \* the state a setter writes
    AtomicCas,  \* TRUE: `compare_and_set` is one step, as every shipped backend's.
                \* FALSE: the trait's default, a `get` then a `set` (flow_state.rs:100)
    NoVal       \* the key is absent from the store

Requests == Gated \cup Pollers \cup Setters \cup Resetters

States == {"S", "A", "B"}
\* Stubs in scan order; the gate of each.  `S` is `INITIAL_SCENARIO_STATE`.
Order == <<"S", "A", "B">>
Successor(s) == CASE s = "S" -> "A"
                  [] s = "A" -> "B"
                  [] s = "B" -> "S"

ASSUME /\ Start \in States \cup {NoVal}
       /\ SetTo \in States
       /\ AtomicCas \in BOOLEAN
       /\ Gated \cap Pollers = {} /\ Gated \cap Setters = {} /\ Gated \cap Resetters = {}
       /\ Pollers \cap Setters = {} /\ Pollers \cap Resetters = {} /\ Setters \cap Resetters = {}

\* `scenario_state` (matching.rs:512): the initial state is normally stored as absence.
Logical(v) == IF v = NoVal THEN "S" ELSE v
Max(a, b) == IF a >= b THEN a ELSE b

VARIABLES
    store,      \* the value the flow store holds for the scenario's key
    map,        \* `scenario_transitions` for this scenario (core/mod.rs:519); 0 = no entry
    clock,      \* `LoadClock`: the last stamp handed out.  The wall clock never advances, the
                \*   worst case the clock exists for (#1301); only the order of stamps matters
    req,        \* [Requests -> the request's own locals]
    ver,        \* ghost: how many writes the store has taken
    writes,     \* ghost: the stamped writes, [ver, to, at]
    served,     \* ghost: the answers given, [ver, answer, stamp]
    maxPub,     \* ghost: the highest stamp ever published to `map`
    casBroken,  \* ghost: a compare-and-set wrote over a value it did not expect
    dropped,    \* ghost: a transition lost its compare-and-set
    noMatch     \* ghost: a request scanned every gate and matched none

vars == <<store, map, clock, req, ver, writes, served, maxPub, casBroken, dropped, noMatch>>

Quiet == \A r \in Requests : req[r].pc \notin {"issue", "publish"}

StartPc(r) ==
    IF r \in Gated \cup Pollers THEN "copy"
    ELSE IF r \in Setters THEN "setRead"
    ELSE "delRead"

\* A request's locals, before it starts and once it is done: nothing can read them after that, so
\* keeping them would only split states.
Locals(pc) == [pc |-> pc, copy |-> 0, i |-> 1, answer |-> "S", rver |-> 0, exp |-> NoVal,
               changed |-> FALSE, wrote |-> 0, to |-> "S", at |-> 0]

Init ==
    /\ store = Start
    /\ map = 0
    /\ clock = 0
    /\ req = [r \in Requests |-> Locals(StartPc(r))]
    /\ ver = 0
    /\ writes = {}
    /\ served = {}
    /\ maxPub = 0
    /\ casBroken = FALSE
    /\ dropped = FALSE
    /\ noMatch = FALSE

\* handler.rs:1359 `imposter.scenario_transitions()`: the copy taken before any gate is read.
Copy(r) ==
    /\ req[r].pc = "copy"
    /\ req' = [req EXCEPT ![r].copy = map, ![r].pc = "gate"]
    /\ UNCHANGED <<store, map, clock, ver, writes, served, maxPub, casBroken, dropped, noMatch>>

\* matching.rs:181-194: one store read per gated candidate, each a separate `get`.  A match at
\* :221 ends the scan; running out of candidates is `Ok((None, trace))` at :230.
GateRead(r) ==
    /\ req[r].pc = "gate"
    /\ LET st == Logical(store)
           i == req[r].i
       IN IF st = Order[i]
          THEN /\ req' = [req EXCEPT ![r].answer = st, ![r].rver = ver, ![r].exp = st,
                                     ![r].to = Successor(st),
                                     ![r].pc = IF r \in Gated THEN "cas" ELSE "serve"]
               /\ UNCHANGED noMatch
          ELSE IF i = Len(Order)
               THEN /\ req' = [req EXCEPT ![r] = Locals("done")]
                    /\ noMatch' = TRUE
               ELSE /\ req' = [req EXCEPT ![r].i = i + 1]
                    /\ UNCHANGED noMatch
    /\ UNCHANGED <<store, map, clock, ver, writes, served, maxPub, casBroken, dropped>>

\* The write a compare-and-set commits.  `exp` is what the caller expects (matching.rs:686, :695).
Commit(r) ==
    /\ store' = req[r].to
    /\ ver' = ver + 1
    /\ casBroken' = (casBroken \/ store # req[r].exp)
    /\ req' = [req EXCEPT ![r].wrote = ver + 1, ![r].pc = "issue"]
    /\ UNCHANGED dropped

\* A compare that failed: retry once expecting absence when the gate was the initial state
\* (matching.rs:692), else the transition is dropped (:705) and the answer still goes out.
Lost(r) ==
    /\ IF store = NoVal /\ req[r].answer = "S"
       THEN /\ req' = [req EXCEPT ![r].exp = NoVal, ![r].pc = "cas"]
            /\ UNCHANGED dropped
       ELSE /\ req' = [req EXCEPT ![r].pc = "serve"]
            /\ dropped' = TRUE
    /\ UNCHANGED <<store, ver, casBroken>>

\* AtomicCas: `InMemoryFlowStore::compare_and_set` (inmemory.rs:287) in one step.
\* Otherwise the default's `get` (flow_state.rs:107), its compare (:108), and `CasSet` next.
CasCompare(r) ==
    /\ req[r].pc = "cas"
    /\ IF store = req[r].exp
       THEN IF AtomicCas
            THEN Commit(r)
            ELSE /\ req' = [req EXCEPT ![r].pc = "casSet"]
                 /\ UNCHANGED <<store, ver, casBroken, dropped>>
       ELSE Lost(r)
    /\ UNCHANGED <<map, clock, writes, served, maxPub, noMatch>>

\* The default's `set` (flow_state.rs:109), with nothing guarding the value since its `get`.
CasSet(r) ==
    /\ req[r].pc = "casSet"
    /\ Commit(r)
    /\ UNCHANGED <<map, clock, writes, served, maxPub, noMatch>>

\* matching.rs:577 `load_clock.next()`.  One atomic step, so every stamp is new.
Issue(r) ==
    /\ req[r].pc = "issue"
    /\ clock' = clock + 1
    /\ writes' = writes \cup {[ver |-> req[r].wrote, to |-> req[r].to, at |-> clock + 1]}
    /\ req' = [req EXCEPT ![r].at = clock + 1, ![r].pc = "publish"]
    /\ UNCHANGED <<store, map, ver, served, maxPub, casBroken, dropped, noMatch>>

\* matching.rs:580-584: the `rcu` retries until it lands, and inserts the stamp issued above,
\* whatever the map holds by then.
Publish(r) ==
    /\ req[r].pc = "publish"
    /\ map' = req[r].at
    /\ maxPub' = Max(maxPub, req[r].at)
    /\ req' = [req EXCEPT ![r] = IF r \in Gated THEN [@ EXCEPT !.pc = "serve"] ELSE Locals("done")]
    /\ UNCHANGED <<store, clock, ver, writes, served, casBroken, dropped, noMatch>>

\* matching.rs:629-650, from handler.rs:2065 and :2415: the stub's own stamp or the copy, whichever
\* is later.  Stubs load before any transition here, so the copy decides.
Serve(r) ==
    /\ req[r].pc = "serve"
    /\ served' = served \cup {[ver |-> req[r].rver, answer |-> req[r].answer,
                               stamp |-> req[r].copy]}
    /\ req' = [req EXCEPT ![r] = Locals("done")]
    /\ UNCHANGED <<store, map, clock, ver, writes, maxPub, casBroken, dropped, noMatch>>

\* matching.rs:537-540: `set_scenario_state` asks whether the write would change the state.
SetRead(r) ==
    /\ req[r].pc = "setRead"
    /\ req' = [req EXCEPT ![r].changed = (Logical(store) # SetTo), ![r].to = SetTo,
                          ![r].pc = "setWrite"]
    /\ UNCHANGED <<store, map, clock, ver, writes, served, maxPub, casBroken, dropped, noMatch>>

\* matching.rs:542-549: the plain write, stamped only when that earlier answer said so.  An
\* ungated stub's answer does not depend on the state, so its serve is not modelled.
SetWrite(r) ==
    /\ req[r].pc = "setWrite"
    /\ store' = SetTo
    /\ ver' = ver + 1
    /\ req' = [req EXCEPT ![r] = IF @.changed
                                  THEN [@ EXCEPT !.wrote = ver + 1, !.pc = "issue"]
                                  ELSE Locals("done")]
    /\ UNCHANGED <<map, clock, writes, served, maxPub, casBroken, dropped, noMatch>>

\* matching.rs:955, :990 and :778: `ScenarioStampingStore::delete` asks `would_change`.
DelRead(r) ==
    /\ req[r].pc = "delRead"
    /\ req' = [req EXCEPT ![r].changed = (store # NoVal), ![r].to = "S", ![r].pc = "delWrite"]
    /\ UNCHANGED <<store, map, clock, ver, writes, served, maxPub, casBroken, dropped, noMatch>>

\* matching.rs:960-963: the delete, stamped only when that earlier answer said so.
DelWrite(r) ==
    /\ req[r].pc = "delWrite"
    /\ store' = NoVal
    /\ ver' = ver + 1
    /\ req' = [req EXCEPT ![r] = IF @.changed
                                  THEN [@ EXCEPT !.wrote = ver + 1, !.pc = "issue"]
                                  ELSE Locals("done")]
    /\ UNCHANGED <<map, clock, writes, served, maxPub, casBroken, dropped, noMatch>>

Next ==
    \E r \in Requests :
        \/ Copy(r) \/ GateRead(r) \/ CasCompare(r) \/ CasSet(r) \/ Issue(r) \/ Publish(r)
        \/ Serve(r) \/ SetRead(r) \/ SetWrite(r) \/ DelRead(r) \/ DelWrite(r)

Spec == Init /\ [][Next]_vars

Symmetry == Permutations(Gated) \cup Permutations(Pollers)
            \cup Permutations(Setters) \cup Permutations(Resetters)

Pcs == {"copy", "gate", "cas", "casSet", "issue", "publish", "serve", "setRead", "setWrite",
        "delRead", "delWrite", "done"}

TypeInv ==
    /\ store \in States \cup {NoVal}
    /\ map \in Nat /\ clock \in Nat /\ ver \in Nat /\ maxPub \in Nat
    /\ \A r \in Requests : req[r].pc \in Pcs
    /\ map <= clock

(***************************************************************************)
(* #1307.  A request whose gate read a state must not carry a stamp at or  *)
(* above the stamp of a later write that moved away from that answer: that *)
(* is the old answer under the new stamp, and its client stays on `304`    *)
(* for the new answer.  `clock` being strictly increasing, and the copy    *)
(* coming before the gate read, is what the property rests on.             *)
(***************************************************************************)
NeverOldAnswerUnderNewStamp ==
    \A x \in served, w \in writes :
        (w.ver > x.ver /\ w.to # x.answer) => x.stamp < w.at

\* #311.  A gated transition never overwrites a state its gate did not see.
TransitionsAreCas == ~casBroken

\* A scenario's stamp only rises: `map` is the highest stamp ever published.
StampMonotone == map = maxPub

(***************************************************************************)
(* What a poller depends on, once nothing is mid-transition: a client that *)
(* holds an answer other than the current one must be told so.  The window *)
(* between a write and its stamp's publish is excluded, because there the  *)
(* client is told `304` for one poll at most, and the publish ends it.     *)
(* `last_modified <= since` (conditional.rs:165) gives the strict `<`.     *)
(***************************************************************************)
NoStaleAnswerAtRest ==
    Quiet => \A x \in served : x.answer # Logical(store) => x.stamp < map

\* Every state has a stub in this table, so a scan that matched none read a state that moved.
NoMatchOnlyWhenUncovered == ~noMatch

\* Witness: must be violated.  A lost compare-and-set is normal concurrency (matching.rs:659).
NeverDroppedTransition == ~dropped

=============================================================================
