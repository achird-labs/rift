-------------------------- MODULE TraceValidation --------------------------
(***************************************************************************)
(* Does a recorded run take an order ProxyOnce allows?                     *)
(*                                                                         *)
(* A trace is the sequence of claim-protocol points a test run recorded,   *)
(* as labels (validate.py reads them from the ndjson that                  *)
(* crates/rift-mock-core/tests/proxyonce_interleavings.rs writes).  The    *)
(* points name no request, so TLC is free to pick which request takes each *)
(* step; the run is accepted when some assignment reaches the trace's end, *)
(* reported as `TraceNotDone` violated.                                    *)
(*                                                                         *)
(* A step with a point is enabled only as the trace's next label.  A step  *)
(* with none is free: a Match, a shadow check that says no, a drop, a      *)
(* settle with no claim, and every swap.  The swaps have to be free: an    *)
(* edit's ProxyOrphanFound fires after the swap, so another task's points  *)
(* can come between the two.                                               *)
(***************************************************************************)
EXTENDS ProxyOnce, Trace

VARIABLE i

Step == Trace[i + 1]
Next1 == IF i < Len(Trace) THEN Step.action ELSE "END"

Lib(label, A) == Next1 = label /\ A /\ i' = i + 1
Env(A) == A /\ UNCHANGED i

\* A try_claim_as, whichever of the two: the outcome is what the request went on to.
Outcome(r) ==
    CASE Step.arg = "Claimed"         -> req'[r].token # NoToken /\ req[r].token = NoToken
      [] Step.arg = "InFlight"        -> req'[r].lost
      [] Step.arg = "AlreadyRecorded" -> req'[r].pc \in {"shadow", "lookup"}

TInit == Init /\ i = 0

TraceNext ==
    \/ \E r \in Requests :
        \/ Env(Match(r))
        \/ Lib("ProxyClaim", (Claim(r) \/ Reclaim(r)) /\ Outcome(r))
        \/ Lib("ProxyShadowed", ShadowCheck(r) /\ req'[r].pc = "forget")
        \/ Env(ShadowCheck(r) /\ req'[r].pc = "lookup")
        \/ Lib("ProxyShadowForgot", ShadowForget(r))
        \/ Lib("ProxyReplayed", Lookup(r) /\ req'[r].pc = "done")
        \/ Lib("ProxyNothingToReplay", Lookup(r) /\ req'[r].pc \in {"reclaim", "forward"})
        \/ Lib("ProxyForwarding", Forward(r))
        \/ Lib("ProxySettled", Settle(r) /\ req[r].token # NoToken)
        \/ Env(Settle(r) /\ req[r].token = NoToken)
        \/ Lib("ProxyInserted", Insert(r) /\ (req[r].token # NoToken) = (Step.arg = "claimed"))
        \/ Env(DropRequest(r))
    \/ \E e \in Editors :
        \/ Env(EditSwap(e))
        \/ Lib("ProxyOrphanForgotten", EditForget(e))
    \/ \E c \in Clearers :
        \/ Lib("ProxyStoreCleared", ClearStore(c))
        \/ Env(ClearSwap(c))
        \/ Lib("ProxyOrphanForgotten", ClearForget(c))

\* Violated exactly when some behaviour of the model takes every step of the trace.
TraceNotDone == i < Len(Trace)
=============================================================================
