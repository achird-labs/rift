-------------------------- MODULE TraceValidation --------------------------
(***************************************************************************)
(* Does a recorded run take an order Scenario allows?                      *)
(*                                                                         *)
(* A trace is the sequence of scenario points one test recorded            *)
(* (validate.py reads them from the ndjson that                            *)
(* crates/rift-mock-core/tests/scenario_interleavings.rs writes), with how *)
(* many requests of each role the run made.  Every test runs on the        *)
(* model's own stub table, so a gate read's position is the model's `i`.   *)
(* The run is accepted when some behaviour reaches the trace's end,        *)
(* reported as `TraceNotDone` violated.                                    *)
(*                                                                         *)
(* A step with a point is enabled only as the trace's next label, and the  *)
(* label constrains what the step does: which branch a compare-and-set     *)
(* took, what a set decided, which stamp was issued, published or served.  *)
(* Stamps are the engine's seconds ranked into the model's counter: the    *)
(* k-th stamp issued is k, and a stub's own load stamp, older than every   *)
(* transition, is 0.  A step with no point is free: the copy of the stamp  *)
(* map, a compare-and-set's retry against absence, a set's write that      *)
(* landed, a clear's map loads and its wipe.                               *)
(*                                                                         *)
(* One point fires before the step it names: a delete's `ScenarioWriteNext` *)
(* comes before its `set_key_ttl`, so a test can hold there.  `told`       *)
(* records the point, and the delete's write needs it.  A clear's fires    *)
(* after its reads, so it labels `ClearHeld`.                              *)
(***************************************************************************)
EXTENDS Scenario, Trace

VARIABLES i, told

Step == Trace[i + 1]
Next1 == IF i < Len(Trace) THEN Step.action ELSE "END"

Lib(label, A) == Next1 = label /\ A /\ i' = i + 1 /\ UNCHANGED told
Env(A) == A /\ UNCHANGED <<i, told>>

Role(name) == CASE name = "gated" -> Gated
                [] name = "poller" -> Pollers
                [] OTHER -> {}

TInit == Init /\ i = 0 /\ told = [r \in Requests |-> FALSE]

TraceNext ==
    \E r \in Requests :
        \/ Lib("GateRead", r \in Role(Step.role) /\ req[r].i = Step.n /\ GateRead(r))
        \/ Lib("CasApplied", CasCompare(r) /\ req'[r].pc = "issue")
        \/ Lib("CasConflict", CasCompare(r) /\ req'[r].pc = "serve")
        \/ Env(CasCompare(r) /\ req'[r].pc \in {"cas", "casSet"})
        \/ Env(CasSet(r))
        \/ Lib("TransitionStampIssued", Issue(r) /\ clock' = Step.n)
        \/ Lib("TransitionStamped", Publish(r) /\ req[r].at = Step.n)
        \/ Lib("Served", Serve(r) /\ req[r].copy = Step.n)
        \/ Lib("ScenarioChangeRead", SetRead(r) /\ req'[r].changed = Step.changed)
        \/ Lib("ScenarioChangeRead", SetWrite(r) /\ req'[r].pc = "setWrite"
                                     /\ req'[r].changed = Step.changed)
        \/ Env(SetWrite(r) /\ req'[r].pc # "setWrite")
        \/ /\ Next1 = "ScenarioWriteNext"
           /\ req[r].pc = "delWrite" /\ ~told[r]
           /\ told' = [told EXCEPT ![r] = TRUE]
           /\ i' = i + 1
           /\ UNCHANGED vars
        \/ Env(DelWrite(r) /\ told[r])
        \/ Lib("ScenarioWriteNext", ClearHeld(r))
        \/ Env(Copy(r) \/ ClearBefore(r) \/ ClearWipe(r) \/ ClearAfter(r))

\* Violated exactly when some behaviour of the model takes every step of the trace.
TraceNotDone == i < Len(Trace)
=============================================================================
