-------------------------- MODULE TraceValidation --------------------------
(***************************************************************************)
(* Does a recorded run take an order FaultIo allows?                       *)
(*                                                                         *)
(* A trace is the sequence of fault points one connection's test recorded  *)
(* (validate.py reads them from the ndjson that                            *)
(* crates/rift-mock-core/tests/faultio_interleavings.rs writes), with the  *)
(* run's configuration: what each request's stub answers, the transport,   *)
(* the HTTP version, when the fault stubs were added and whether the       *)
(* client pipelines.  The run is accepted when some behaviour from that    *)
(* configuration reaches the trace's end, reported as `TraceNotDone`       *)
(* violated.                                                               *)
(*                                                                         *)
(* A step with a point is enabled only as the trace's next label, and its  *)
(* label's arguments constrain it.  A step with none is free: reads,       *)
(* writes of response bytes, flushes, the client, the kernel.  A step      *)
(* that would fire a point the trace does not have next is not free: it is *)
(* the engine taking a step the run did not take.                          *)
(***************************************************************************)
EXTENDS FaultIo, Trace

VARIABLE i

Step == Trace[i + 1]
Next1 == IF i < Len(Trace) THEN Step.action ELSE "END"

Lib(label, A) == Next1 = label /\ A /\ i' = i + 1
Env(A) == A /\ UNCHANGED i

\* What fires at the service's end (manager.rs:208-211): an armed carrier, or a lingering close.
Arms == svc = "computing" /\ KindOf(current) \in Carriers
Lingers == svc = "computing" /\ KindOf(current) = "refuse"

\* A write that finds the cell armed takes it (`FaultCell::take`, fault_io.rs:212).
Trips == cell # None

\* The run's configuration, from its trace's Start line.
TraceConfig ==
    /\ script = TScript
    /\ https = THttps
    /\ version = TVersion
    /\ timing = TTiming
    /\ pipelines = TPipelines

TInit == Init /\ TraceConfig /\ i = 0

TraceNext ==
    \/ Lib("FaultArmed", ServiceDone /\ Arms /\ cell' = Step.arg)
    \/ Lib("LingerArmed", ServiceDone /\ Lingers /\ linger')
    \/ Env(ServiceDone /\ ~Arms /\ ~Lingers)
    \/ Lib("FaultTaken", HttpWrite /\ Trips /\ cell = Step.arg /\ Step.layer = "RawSocket")
    \/ Env(HttpWrite /\ ~Trips)
    \/ Lib("FaultTaken", TlsRecordWrite /\ Trips /\ cell = Step.arg /\ Step.layer = "RawSocket")
    \/ Env(TlsRecordWrite /\ ~Trips)
    \/ Lib("FaultTaken", TlsTakePayload /\ cell = Step.arg /\ Step.layer = "AboveTls")
    \/ Lib("LingerFinSent", LingerShutWrite)
    \/ Lib("LingerDrained", DrainEnds)
    \/ Env(AddFaultStub \/ ClientSend \/ ClientWriteBody \/ ClientCloses \/ ReadHead
           \/ WriteHead \/ DropBody \/ TlsSeal \/ TlsPayloadSeal \/ TlsTripped \/ FlushDone
           \/ PlainShutdown \/ DrainRead \/ Close \/ Deliver)
    \/ \E r \in Reqs :
        \/ Env(H2Dispatch(r) \/ H2ServiceDone(r))
        \/ Env(H2Write(r) /\ ~Trips)
        \/ Lib("FaultTaken", H2Write(r) /\ Trips /\ cell = Step.arg /\ Step.layer = "RawSocket")

\* Violated exactly when some behaviour of the model takes every step of the trace.
TraceNotDone == i < Len(Trace)
=============================================================================
