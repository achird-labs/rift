-------------------------- MODULE TraceValidation --------------------------
(***************************************************************************)
(* Does a recorded run take an order Lifecycle allows?                     *)
(*                                                                         *)
(* A trace is the sequence of lifecycle steps a test run took, as action   *)
(* labels (trace_labels.py derives them from the probe points the run      *)
(* recorded, crates/rift-mock-core/src/imposter/probe.rs).  validate.py    *)
(* writes it as the module `Trace`: `Trace`, a sequence of records         *)
(* [action, op, port, n, ok], and `OpPort`, each op's port.  Each action   *)
(* of the model that has a probe point is enabled only when it is the      *)
(* trace's next label, for that op, and advances the cursor; the steps     *)
(* with no point (a lock being taken, an op returning, a request starting  *)
(* or being written) are free.  The trace is accepted when some behaviour  *)
(* reaches its end: TLC reports `TraceNotDone` violated, which validate.py *)
(* reads as acceptance.                                                    *)
(***************************************************************************)
EXTENDS Lifecycle, Trace, Sequences

VARIABLE i

Step == Trace[i + 1]
Next1 == IF i < Len(Trace) THEN Step.action ELSE "END"
StepGen == <<Step.port, Step.n>>

Lib(label, o, A) == Next1 = label /\ Step.op = o /\ A /\ i' = i + 1
Env(A) == A /\ UNCHANGED i

TInit ==
    /\ Init
    /\ \A o \in Ops : op[o].port = OpPort[o]
    /\ i = 0

TraceNext ==
    \/ \E o \in Ops :
        \* Creates.  The generation the engine numbered is the port's next one in the model.
        \/ Lib("CreateBindOk", o, CreateBindOk(o) /\ op'[o].gen = StepGen)
        \/ Lib("CreateReadFloor", o, CreateReadFloor(o))
        \/ Lib("CreateSpawnAccept", o, CreateSpawnAccept(o))
        \/ Lib("CreateTryClaim", o, CreateTryClaim(o) /\ (op'[o].pc = "cPersist") = Step.ok)
        \/ Env(CreatePersistLock(o))
        \/ Lib("CreatePersistWrite", o, CreatePersistWrite(o) /\ (op'[o].pc = "cRollback") = ~Step.ok)
        \/ Lib("CreateRollback", o, CreateRollback(o))
        \* Deletes.  `ok` is whether the step found what it looked for.
        \/ Lib("DeleteLock", o, DeleteLock(o))
        \/ Lib("DeleteLookup", o, DeleteLookup(o) /\ (op'[o].pc = "dUnlink") = Step.ok
                                  /\ (Step.ok => op'[o].look = StepGen))
        \/ Lib("DeleteUnlink", o, DeleteUnlink(o) /\ (op'[o].pc = "dCancel") = Step.ok)
        \/ Lib("DeleteRecordFloor", o, DeleteRecordFloor(o))
        \/ Lib("DeleteRemoveIfSame", o, DeleteRemoveIfSame(o) /\ (op'[o].pc = "dDrain") = Step.ok)
        \/ Lib("ReplaceLookup", o, ReplaceLookup(o) /\ (op'[o].pc = "dCancel") = Step.ok
                                   /\ (Step.ok => op'[o].look = StepGen))
        \/ Lib("DeleteCancel", o, DeleteCancel(o))
        \/ Lib("DeleteDrain", o, DeleteDrain(o))
        \/ Lib("DeleteClearShared", o, DeleteClearShared(o))
        \/ Lib("DeleteFinish", o, DeleteFinish(o))
        \/ Env(ReplaceLock(o))
        \/ Env(DeleteReturn(o))
        \* Sweeps.
        \/ Lib("ApplySnapshot", o, ApplySnapshot(o) /\ Cardinality(op'[o].plan) = Step.n)
        \/ Env(ApplySweep(o)) \/ Env(ApplyEntry(o))
        \/ Env(ReplaceAfterDelete(o)) \/ Env(ReplaceAfterCreate(o))
        \* A create refused at the bind check records no point.
        \/ Env(CreateBindBusy(o)) \/ Env(CreateBindBound(o))
    \/ \E g \in GenIds :
        \/ Env(ReqStart(g))
        \/ Next1 = "Served" /\ g = StepGen /\ ReqCompute(g) /\ i' = i + 1
        \/ Env(ReqWrite(g))
        \/ Env(ContentChange(g))

TraceSpec == TInit /\ [][TraceNext]_<<vars, i>>

\* Violated exactly when some behaviour of the model takes every step of the trace.
TraceNotDone == i < Len(Trace)
=============================================================================
