-------------------------- MODULE TraceValidation --------------------------
(***************************************************************************)
(* Does a recorded run take an order StateCell allows?                     *)
(*                                                                         *)
(* A trace is the sequence of state cell points one test recorded          *)
(* (validate.py reads them from the ndjson that                            *)
(* crates/rift-mock-core/tests/scripting_interleavings.rs writes), with    *)
(* how many script runs started, restarts and access changes the run made. *)
(* The run is accepted when some behaviour reaches the trace's end,        *)
(* reported as `TraceNotDone` violated.                                    *)
(*                                                                         *)
(* Four points against the model's many actions: a start that builds the   *)
(* live state from the cell, and the three steps of a run's end.  Every    *)
(* other step is free: the caller's, the queue's, jobs and timers, a start *)
(* that finds the live state, deferral, how a run in flight ends, restarts *)
(* and access changes.  A step that would fire a point is never free.      *)
(*                                                                         *)
(* A point names its cell by a key, written as the order in which the cell *)
(* first appeared.  `cellOf` binds each key to a model cell the first time *)
(* a step takes it, one key to one cell, and every later step with that    *)
(* key must be a run of that cell: so TLC cannot credit one cell's save to *)
(* a run of another.                                                       *)
(***************************************************************************)
EXTENDS StateCell, Trace

VARIABLES i, cellOf

Step == Trace[i + 1]
Next1 == IF i < Len(Trace) THEN Step.action ELSE "END"

\* The key of the next step names run r's cell.
Bind(r) ==
    LET k == Step.key
        c == run[r].cell
    IN IF cellOf[k] = 0
       THEN /\ \A j \in DOMAIN cellOf : cellOf[j] # c
            /\ cellOf' = [cellOf EXCEPT ![k] = c]
       ELSE /\ cellOf[k] = c
            /\ UNCHANGED cellOf

Lib(label, r, A) == Next1 = label /\ A /\ Bind(r) /\ i' = i + 1
Env(A) == A /\ UNCHANGED <<i, cellOf>>

TInit == Init /\ i = 0 /\ cellOf = [k \in 1..TKeys |-> 0]

TraceNext ==
    \/ \E r \in Runs :
        \/ Lib("ScriptSeeded", r, Start(r) /\ KeyOf(r) \notin present)
        \/ Env(Start(r) /\ KeyOf(r) \in present)
        \/ Lib("ScriptSaved", r, EndSave(r))
        \/ Lib("ScriptFinished", r, FinishAnswered(r))
        \/ Lib("ScriptReleased", r, EndRelease(r))
        \/ Env(Capture(r) \/ Submit(r) \/ CallerLeaves(r) \/ StartAbandoned(r) \/ Defer(r)
               \/ OwnJob(r) \/ InheritedJob(r) \/ Finish(r) \/ Deadline(r) \/ Abandoned(r)
               \/ Idle(r))
    \/ Env(Restart \/ SetAccess)

\* Violated exactly when some behaviour of the model takes every step of the trace.
TraceNotDone == i < Len(Trace)
=============================================================================
