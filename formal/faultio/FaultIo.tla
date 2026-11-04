------------------------------ MODULE FaultIo ------------------------------
(***************************************************************************)
(* Connection fault injection on one accepted imposter connection:        *)
(* how a `FaultCell` is armed, which write takes it, and how the connection *)
(* ends (crates/rift-mock-core/src/imposter/fault_io.rs, the arm site in   *)
(* `run_http1`, manager.rs:208).                                           *)
(*                                                                         *)
(* The environment is hyper 1.12.0's HTTP/1 dispatcher and the kernel,     *)
(* because the fault's correctness rests on them.  Each action's comment   *)
(* names the line it models; "hyper" paths are under                       *)
(* hyper-1.12.0/src/proto/h1/.  Each action is one step between two points *)
(* where another step can run.  A byte is a unit, a response is two units  *)
(* (head, body), and a socket that cannot take a byte is an action that    *)
(* does not run yet.                                                       *)
(*                                                                         *)
(* Ghost flags record a property violation at the step that commits it,   *)
(* so an invariant is one flag and TLC's trace ends on the guilty step.    *)
(***************************************************************************)
EXTENDS Naturals, Sequences, TLC

CONSTANTS
    MaxReqs,            \* requests the client sends on this connection
    Kinds,              \* what a request's stub may answer: "ok", "refuse" or a fault kind
    Transports,         \* {"http", "https"}: whether the stream is under TLS
    ClientVersions,     \* {"h1", "h2"}: what the client speaks where the listener offers both
    StubTimings,        \* {"before", "after"}: the fault stubs exist at accept, or are added later
    ClientPipelines,    \* {TRUE, FALSE}: the client sends before it has read the last response
    BodyUnits,          \* units of request body a refused request still has to write
    PipelineFlush,      \* hyper's `pipeline_flush` option.  FALSE: Rift never sets it (io.rs:279)
    ImposterArmsOnH2,   \* TRUE: `run_http1` arms for any version (manager.rs:208).  FALSE:
                        \* `apply_tcp_fault`'s rule, as on the admin and front-door listeners
    DrainOutrunsDelivery \* TRUE: the drain's 2 s bound may end before the kernel delivered its queue

Carriers == {"reset", "empty", "random", "malformed"}
Payloads == {"random", "malformed"}
Reqs == 1..MaxReqs
None == "none"

ASSUME /\ MaxReqs >= 1 /\ BodyUnits >= 0
       /\ Kinds \subseteq Carriers \cup {"ok", "refuse"}
       /\ Transports \subseteq {"http", "https"}
       /\ ClientVersions \subseteq {"h1", "h2"}
       /\ StubTimings \subseteq {"before", "after"}
       /\ ClientPipelines \subseteq BOOLEAN

VARIABLES
    script,     \* request -> what its stub answers, fixed per behaviour
    https,      \* the stream is under TLS
    version,    \* "h1" or "h2", fixed when the connection is accepted
    timing,     \* "before" or "after", as `StubTimings`
    pipelines,  \* as `ClientPipelines`, fixed per behaviour
    hasStub,    \* the imposter's stubs can fault now
    sent,       \* requests the client has sent
    current,    \* the last request hyper read a head for
    svc,        \* hyper's in-flight service future: "idle", "computing", "ready"
    reading,    \* hyper's `Reading`: "Init", "Body", "KeepAlive", "Closed"
    writing,    \* hyper's `Writing`: "Init", "KeepAlive", "Closed"
    wbuf,       \* hyper's write buffer
    tlsBuf,     \* the rustls session's unwritten records
    cell,       \* the `FaultCell`'s slot
    linger,     \* the `FaultCell`'s lingering-close flag
    trip,       \* `TlsPayloadFault.trip`: "none", "writing", "flushing", "tripped"
    phase,      \* "serving", "aborted", "draining", "closing", "closed"
    soLinger,   \* SO_LINGER(0) is set: the close sends RST
    sendq,      \* the kernel's send queue
    wire,       \* what has reached the client, in order
    unread,     \* request-body units in the kernel's receive queue
    bodyLeft,   \* request-body units the client has yet to write
    served,     \* request -> what it was answered, None until its service finished
    streams,    \* request -> HTTP/2 stream state
    ghost

vars == <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading, writing,
          wbuf, tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, unread, bodyLeft, served,
          streams, ghost>>

\* A unit in a buffer: <<"b", r>> a byte of response r; <<"tls", 0>> a payload fault inside TLS;
\* <<"raw", 0>> a payload fault beneath TLS; <<"fin", 0>>, <<"rst", 0>> the end of the stream.
Byte(r) == <<"b", r>>
Tag(e) == e[1]
Items(s) == {s[i] : i \in 1..Len(s)}
On(tag, s) == \E e \in Items(s) : Tag(e) = tag
Whole(r) == Len(SelectSeq(wire, LAMBDA e : e = Byte(r))) = 2

\* The kind a request's response has once its stub has been evaluated: a fault stub that does not
\* exist yet cannot answer.
KindOf(r) == IF script[r] \in Carriers /\ ~hasStub THEN "ok" ELSE script[r]

Init ==
    /\ script \in [Reqs -> Kinds]
    /\ https \in {t = "https" : t \in Transports}
    /\ timing \in StubTimings
    /\ pipelines \in ClientPipelines
    /\ hasStub = (timing = "before")
    \* `http1_only` is evaluated once, at accept (manager.rs:1971), from the stubs of that moment.
    /\ version \in IF hasStub THEN {"h1"} ELSE ClientVersions
    /\ sent = 0
    /\ current = 0
    /\ svc = "idle"
    /\ reading = "Init"
    /\ writing = "Init"
    /\ wbuf = <<>>
    /\ tlsBuf = <<>>
    /\ cell = None
    /\ linger = FALSE
    /\ trip = "none"
    /\ phase = "serving"
    /\ soLinger = FALSE
    /\ sendq = <<>>
    /\ wire = <<>>
    /\ unread = 0
    /\ bodyLeft = BodyUnits
    /\ served = [r \in Reqs |-> None]
    /\ streams = [r \in Reqs |-> "idle"]
    /\ ghost = [fires |-> 0, rawPayloadFirst |-> FALSE, armedAfterWrite |-> FALSE,
                trippedOnEarlier |-> FALSE, wroteAny |-> FALSE, collateral |-> FALSE,
                discarded |-> FALSE, payloadLost |-> FALSE]

Ended == On("rst", wire) \/ On("fin", wire)

(***************************************************************************)
(* The client and the imposter's stubs.                                    *)
(***************************************************************************)

\* A stub write under a live connection (manager.rs:2844): nothing re-evaluates `http1_only`.
AddFaultStub ==
    /\ ~hasStub
    /\ hasStub' = TRUE
    /\ UNCHANGED <<script, https, version, timing, pipelines, sent, current, svc, reading, writing,
                   wbuf, tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* A pipelining client (and any HTTP/2 client) sends at will; another waits for the last response.
ClientSend ==
    /\ sent < MaxReqs
    /\ ~Ended
    /\ \/ pipelines \/ version = "h2" \/ sent = 0
       \/ Len(SelectSeq(wire, LAMBDA e : e = Byte(sent))) = 2
    /\ sent' = sent + 1
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, unread,
                   bodyLeft, served, streams, ghost>>

\* The rest of a refused request's body: the client keeps writing whatever the server answered, as
\* long as it has not been reset (fault_io.rs:199).
ClientWriteBody ==
    /\ version = "h1"
    /\ current > 0 /\ script[current] = "refuse"
    /\ reading = "Body" \/ served[current] = "refuse"
    /\ bodyLeft > 0
    /\ ~On("rst", wire)
    /\ phase # "closed"
    /\ bodyLeft' = bodyLeft - 1
    /\ unread' = unread + 1
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, served,
                   streams, ghost>>

\* The client closes an idle connection instead of sending the rest; both counters jump, as nothing
\* more will be sent or read.  hyper reads EOF with no message started (dispatch.rs:343) and closes
\* both sides (`maybe_notify`, conn.rs:548).
ClientCloses ==
    /\ version = "h1" /\ phase = "serving"
    /\ reading = "Init" /\ writing = "Init" /\ svc = "idle"
    /\ current = sent
    /\ sent' = MaxReqs
    /\ current' = MaxReqs
    /\ reading' = "Closed"
    /\ writing' = "Closed"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, svc, wbuf, tlsBuf, cell,
                   linger, trip, phase, soLinger, sendq, wire, unread, bodyLeft, served, streams,
                   ghost>>

(***************************************************************************)
(* hyper's HTTP/1 dispatcher.                                              *)
(***************************************************************************)

\* `can_read_head` (conn.rs:180) needs `Reading::Init`, and `poll_ready` (dispatch.rs:634) needs no
\* service in flight.  Nothing else stops the next head: that `Reading::Init` is the whole gate.
ReadHead ==
    /\ version = "h1" /\ phase = "serving"
    /\ reading = "Init" /\ svc = "idle"
    /\ current < sent
    /\ current' = current + 1
    /\ svc' = "computing"
    \* Bodies below the limits are read before the handler answers (handler.rs:1131), so only the
    \* refusal leaves one unread.
    /\ reading' = IF script[current + 1] = "refuse" THEN "Body" ELSE "KeepAlive"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, writing, wbuf, tlsBuf,
                   cell, linger, trip, phase, soLinger, sendq, wire, unread, bodyLeft, served,
                   streams, ghost>>

\* The service future completes: `run_http1` arms the cell for a carrier (manager.rs:208) and the
\* lingering close for a refusal (manager.rs:211, fault_io.rs:263), before hyper sees the response.
ServiceDone ==
    /\ version = "h1" /\ phase = "serving"
    /\ svc = "computing"
    /\ LET k == KindOf(current) IN
       /\ served' = [served EXCEPT ![current] = k]
       /\ cell' = IF k \in Carriers THEN k ELSE cell
       /\ linger' = (linger \/ k = "refuse")
       /\ ghost' = [ghost EXCEPT !.armedAfterWrite = @ \/ (k \in Carriers /\ ghost.wroteAny)]
    /\ svc' = "ready"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, reading,
                   writing, wbuf, tlsBuf, trip, phase, soLinger, sendq, wire, unread, bodyLeft,
                   streams>>

\* `poll_msg` hands hyper the response (dispatch.rs:364): head and body into the write buffer, no
\* I/O.  A `connection: close` response leaves `Writing::Closed` (conn.rs:602, `write_body_and_end`
\* conn.rs:781).
WriteHead ==
    /\ version = "h1" /\ phase = "serving"
    /\ svc = "ready" /\ writing = "Init"
    /\ wbuf' = wbuf \o <<Byte(current), Byte(current)>>
    /\ svc' = "idle"
    /\ writing' = IF served[current] = "refuse" THEN "Closed" ELSE "KeepAlive"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, reading,
                   tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* The handler returned with the body unread, so the receiver is gone: hyper stops reading
\* (dispatch.rs:239, `poll_drain_or_close_read` conn.rs:876).
DropBody ==
    /\ version = "h1" /\ phase = "serving"
    /\ reading = "Body" /\ served[current] = "refuse"
    /\ reading' = "Closed"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, writing,
                   wbuf, tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* `flush_pipeline && !read_buf.is_empty()` makes `poll_flush` answer Ready without writing
\* (io.rs:279).  Rift never sets the option.
FlushSkipped == PipelineFlush /\ sent > current

\* Bytes of a response other than a fault's own carrier, still inside the process when a fault
\* takes the connection.
EarlierInProcess ==
    \E e \in Items(wbuf) \cup Items(tlsBuf) :
        Tag(e) = "b" /\ served[e[2]] \notin Carriers

\* `FaultIo::trip` (fault_io.rs:404) on the raw socket: SO_LINGER(0) for a reset, a best-effort
\* `try_write` for a payload (`Err` only logged, fault_io.rs:419), and the error that aborts hyper
\* (dispatch.rs:451, then :128).  The rest of the buffers are never written.
TripRaw(k) ==
    /\ cell' = None     \* FaultCell::take
    /\ phase' = "aborted"
    /\ soLinger' = (k = "reset")
    /\ \/ /\ k \in Payloads
          /\ sendq' = Append(sendq, <<"raw", 0>>)
       \/ sendq' = sendq

\* Follows TripRaw in a step: whether the payload went out is read from that step's `sendq'`.
TripGhost(k) ==
    ghost' = [ghost EXCEPT
        !.fires = @ + 1,
        !.rawPayloadFirst = @ \/ (https /\ k \in Payloads /\ ghost.fires = 0),
        !.trippedOnEarlier = @ \/ EarlierInProcess,
        !.payloadLost = @ \/ (k \in Payloads /\ sendq' = sendq)]

\* hyper's flush over plain http: `FaultIo::poll_write` (fault_io.rs:448).  Flatten strategy
\* (io.rs:318), since `FaultIo` is not vectored.
HttpWrite ==
    /\ version = "h1" /\ phase = "serving"
    /\ ~https
    /\ wbuf # <<>>
    /\ ~FlushSkipped
    /\ IF cell = None
       THEN /\ sendq' = Append(sendq, Head(wbuf))
            /\ wbuf' = Tail(wbuf)
            /\ ghost' = [ghost EXCEPT !.wroteAny = TRUE]
            /\ UNCHANGED <<cell, phase, soLinger>>
       ELSE /\ TripRaw(cell)
            /\ TripGhost(cell)
            /\ UNCHANGED wbuf
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, tlsBuf, linger, trip, wire, unread, bodyLeft, served, streams>>

\* `TlsPayloadFault::poll_fault_on_write` (fault_io.rs:582): on hyper's write, above TLS, a payload
\* kind is taken (`take_payload`, fault_io.rs:225) instead of writing.
TlsTakePayload ==
    /\ version = "h1" /\ phase = "serving"
    /\ https
    /\ wbuf # <<>>
    /\ ~FlushSkipped
    /\ trip = "none"
    /\ cell \in Payloads
    /\ cell' = None
    /\ trip' = "writing"
    /\ ghost' = [ghost EXCEPT !.fires = @ + 1, !.trippedOnEarlier = @ \/ EarlierInProcess]
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, linger, phase, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams>>

\* Otherwise the bytes pass through into the rustls session (fault_io.rs:617).
TlsSeal ==
    /\ version = "h1" /\ phase = "serving"
    /\ https
    /\ wbuf # <<>>
    /\ ~FlushSkipped
    /\ trip = "none"
    /\ cell \notin Payloads
    /\ tlsBuf' = Append(tlsBuf, Head(wbuf))
    /\ wbuf' = Tail(wbuf)
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, cell, linger, trip, phase, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* `Trip::Writing` (fault_io.rs:532): the payload through the TLS stream, as application data.
TlsPayloadSeal ==
    /\ version = "h1" /\ phase = "serving"
    /\ trip = "writing"
    /\ tlsBuf' = Append(tlsBuf, <<"tls", 0>>)
    /\ trip' = "flushing"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, cell, linger, phase, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* tokio-rustls writes its records to `FaultIo` (fault_io.rs:448), which takes what is still armed:
\* a reset or an empty close, beneath TLS.
TlsRecordWrite ==
    /\ version = "h1" /\ phase = "serving"
    /\ tlsBuf # <<>>
    /\ IF cell = None
       THEN /\ sendq' = Append(sendq, Head(tlsBuf))
            /\ tlsBuf' = Tail(tlsBuf)
            /\ ghost' = [ghost EXCEPT !.wroteAny = TRUE]
            /\ UNCHANGED <<cell, phase, soLinger>>
       ELSE /\ TripRaw(cell)
            /\ TripGhost(cell)
            /\ UNCHANGED tlsBuf
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, linger, trip, wire, unread, bodyLeft, served, streams>>

\* `Trip::Flushing` then `Tripped` (fault_io.rs:553, :529): the records are out, the abort error
\* goes to hyper, and no `close_notify` follows.
TlsTripped ==
    /\ version = "h1" /\ phase = "serving"
    /\ trip = "flushing" /\ tlsBuf = <<>>
    /\ trip' = "tripped"
    /\ phase' = "aborted"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* `Conn::poll_flush` (conn.rs:855): only once `Buffered::poll_flush` is Ready does
\* `try_keep_alive` run (conn.rs:1100), and only that returns `Reading` to `Init`.  This gate is the
\* answer to "can a fault armed for N+1 trip on the bytes of N": the head of N+1 is not read while
\* N is in hyper's buffer.  With `pipeline_flush` the Ready comes with the bytes still there.
FlushDone ==
    /\ version = "h1" /\ phase = "serving"
    /\ trip = "none"
    /\ (wbuf = <<>> /\ tlsBuf = <<>>) \/ FlushSkipped
    /\ \/ /\ reading = "KeepAlive" /\ writing = "KeepAlive"
          /\ reading' = "Init" /\ writing' = "Init"
       \/ /\ \/ reading = "Closed" /\ writing = "KeepAlive"
             \/ reading = "KeepAlive" /\ writing = "Closed"
          /\ reading' = "Closed" /\ writing' = "Closed"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, wbuf,
                   tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams, ghost>>

(***************************************************************************)
(* The end of the connection.                                              *)
(***************************************************************************)

\* Both sides closed: `is_done` (dispatch.rs:469), then `Buffered::poll_shutdown`, which flushes
\* first (io.rs:337), then `FaultIo::poll_shutdown` (fault_io.rs:458), unarmed.
PlainShutdown ==
    /\ version = "h1" /\ phase = "serving"
    /\ reading = "Closed" /\ writing = "Closed"
    /\ wbuf = <<>> /\ tlsBuf = <<>>
    /\ ~linger
    /\ sendq' = Append(sendq, <<"fin", 0>>)
    /\ phase' = "closing"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, soLinger, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* Armed: `Linger::ShuttingWrite` sends the FIN (fault_io.rs:469) before the drain starts.
LingerShutWrite ==
    /\ version = "h1" /\ phase = "serving"
    /\ reading = "Closed" /\ writing = "Closed"
    /\ wbuf = <<>> /\ tlsBuf = <<>>
    /\ linger
    /\ sendq' = Append(sendq, <<"fin", 0>>)
    /\ phase' = "draining"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, soLinger, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* `Drain::poll` (fault_io.rs:363) reads and discards what the client sends.
DrainRead ==
    /\ phase = "draining"
    /\ unread > 0
    /\ unread' = unread - 1
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, bodyLeft,
                   served, streams, ghost>>

\* The drain ends: a timer, the byte backstop, EOF or a read error (fault_io.rs:365-382), all the
\* same here.  The bounds are seconds; delivering a small response takes much less unless the client
\* has stopped reading too (`DrainOutrunsDelivery`, README.md, "Documented limits").
DrainEnds ==
    /\ phase = "draining"
    /\ sendq = <<>> \/ DrainOutrunsDelivery
    /\ phase' = "closing"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams, ghost>>

\* The stream is dropped.  The kernel answers a close with RST under SO_LINGER(0) or with unread
\* data in the receive queue, and either drops its send queue.  Requests pipelined past the current
\* one are in hyper's read buffer (read, so harmless) or in the kernel (unread): both can happen.
Close ==
    /\ phase \in {"aborted", "closing"}
    /\ \E tailUnread \in IF version = "h1" /\ sent > current THEN BOOLEAN ELSE {FALSE} :
       LET rst == soLinger \/ unread > 0 \/ tailUnread IN
       /\ IF rst
          THEN /\ sendq' = <<>>
               /\ wire' = Append(wire, <<"rst", 0>>)
               /\ ghost' = [ghost EXCEPT !.discarded = @ \/ sendq # <<>>]
          ELSE /\ sendq' = IF On("fin", sendq) \/ On("fin", wire)
                           THEN sendq ELSE Append(sendq, <<"fin", 0>>)
               /\ UNCHANGED <<wire, ghost>>
    /\ phase' = "closed"
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, soLinger, unread, bodyLeft, served,
                   streams>>

\* The kernel delivers its queue in order, whatever the process does.
Deliver ==
    /\ sendq # <<>>
    /\ wire' = Append(wire, Head(sendq))
    /\ sendq' = Tail(sendq)
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, phase, soLinger, unread, bodyLeft,
                   served, streams, ghost>>

(***************************************************************************)
(* HTTP/2: streams multiplexed on one socket and one cell.  Reached only   *)
(* by a connection accepted while the imposter had no fault stub.          *)
(***************************************************************************)

H2Dispatch(r) ==
    /\ version = "h2" /\ phase = "serving"
    /\ r <= sent /\ streams[r] = "idle"
    /\ streams' = [streams EXCEPT ![r] = "computing"]
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, phase, soLinger, sendq, wire, unread,
                   bodyLeft, served, ghost>>

\* `run_http1`'s service arms the connection's cell whatever the version (manager.rs:208).  The
\* other listeners go through `apply_tcp_fault` (gateway.rs:38): the service errs, and hyper's h2
\* server resets that stream alone (hyper proto/h2/server.rs:465).
H2ServiceDone(r) ==
    /\ version = "h2" /\ phase = "serving"
    /\ streams[r] = "computing"
    /\ LET k == KindOf(r) IN
       /\ served' = [served EXCEPT ![r] = k]
       /\ IF k \in Carriers /\ ~ImposterArmsOnH2
          THEN /\ streams' = [streams EXCEPT ![r] = "reset"]
               /\ UNCHANGED cell
          ELSE /\ streams' = [streams EXCEPT ![r] = "ready"]
               /\ cell' = IF k \in Carriers THEN k ELSE cell
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, linger, trip, phase, soLinger, sendq, wire, unread,
                   bodyLeft, ghost>>

\* A frame of any stream reaches `FaultIo::poll_write`; an armed cell takes the whole socket, and
\* with it every stream still in flight.
H2Write(r) ==
    /\ version = "h2" /\ phase = "serving"
    /\ streams[r] = "ready"
    /\ IF cell = None
       THEN /\ streams' = [streams EXCEPT ![r] = "answered"]
            /\ UNCHANGED <<cell, phase, soLinger, ghost>>
       ELSE /\ cell' = None
            /\ phase' = "aborted"
            /\ soLinger' = (cell = "reset")
            /\ streams' = [s \in Reqs |->
                 IF streams[s] \in {"computing", "ready"} THEN "lost" ELSE streams[s]]
            /\ ghost' = [ghost EXCEPT
                 !.fires = @ + 1,
                 !.collateral = @ \/ \E s \in Reqs :
                    streams[s] \in {"computing", "ready"} /\ served[s] \notin Carriers]
    /\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, linger, trip, sendq, wire, unread, bodyLeft, served>>

Next ==
    \/ AddFaultStub \/ ClientSend \/ ClientWriteBody \/ ClientCloses
    \/ ReadHead \/ ServiceDone \/ WriteHead \/ DropBody
    \/ HttpWrite \/ TlsTakePayload \/ TlsSeal \/ TlsPayloadSeal \/ TlsRecordWrite \/ TlsTripped
    \/ FlushDone \/ PlainShutdown \/ LingerShutWrite \/ DrainRead \/ DrainEnds \/ Close \/ Deliver
    \/ \E r \in Reqs : H2Dispatch(r) \/ H2ServiceDone(r) \/ H2Write(r)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Properties.                                                             *)
(***************************************************************************)

TypeInv ==
    /\ phase \in {"serving", "aborted", "draining", "closing", "closed"}
    /\ cell \in Carriers \cup {None}
    /\ reading \in {"Init", "Body", "KeepAlive", "Closed"}
    /\ writing \in {"Init", "KeepAlive", "Closed"}
    /\ svc \in {"idle", "computing", "ready"}

\* One armed fault, one firing (`FaultCell::take`, fault_io.rs:212).
FaultFiresAtMostOnce == ghost.fires <= 1

\* On https the first layer to see a payload kind is the one above TLS (#1600).
PayloadAboveTlsOnHttps == ~ghost.rawPayloadFirst

\* No byte of a fault's carrier ever reaches the kernel (fault_io.rs:6).
FaultBeforeResponseBytes ==
    \A e \in Items(sendq) \cup Items(wire) :
        Tag(e) = "b" => served[e[2]] \notin Carriers

\* A fault armed for a later response never takes the connection while an earlier response's bytes
\* are still in the process: the pipelining question.
FaultNeverTripsEarlierBytes == ~ghost.trippedOnEarlier

\* A reset under a refusal comes after the refusal and its FIN (#1460).
LingerDeliversResponse ==
    (version = "h1" /\ On("rst", wire) /\ \E r \in Reqs : served[r] = "refuse") =>
        /\ On("fin", wire)
        /\ \A r \in Reqs : served[r] = "refuse" => Whole(r)

\* The fault of one HTTP/2 stream takes no other stream (fault_io.rs:183, gateway.rs:25).  #1782.
H2FaultStaysOnStream == ~ghost.collateral

\* A close that sends RST discards nothing the kernel has not delivered yet.  #1783.
RstDiscardsNothing == ~ghost.discarded

(***************************************************************************)
(* Witnesses: documented limits, each expected to be violated (witness.py). *)
(***************************************************************************)

\* A fault is not only ever a connection's first write: on a kept-alive connection it is armed after
\* earlier responses went out, which is why the ordering above needs proving.
NeverArmAfterFirstWrite == ~ghost.armedAfterWrite


\* A close that is not a reset fault, and not a refusal, ends in a FIN.
GracefulFaultEndsInFin == (~soLinger /\ ~linger) => ~On("rst", wire)

\* A payload kind reaches the kernel.
PayloadAlwaysWritten == ~ghost.payloadLost

=============================================================================
