# Connection fault injection model

`FaultIo.tla` models one accepted imposter connection: how a `FaultCell` is armed, which write takes it, and how the
connection ends. It covers `FaultCell`, `FaultIo`, `TlsPayloadFault` and the lingering close
(`crates/rift-mock-core/src/imposter/fault_io.rs`) and the arm site in `run_http1` (`manager.rs:208`). The environment
is hyper 1.12.0's HTTP/1 dispatcher, rustls and the kernel, since the fault's correctness rests on them. Every action
was written from the code it cites, and hyper's from the source at the version in `Cargo.lock`.

```sh
JAR=~/.vscode/extensions/tlaplus.vscode-ide-*/tools/tla2tools.jar    # or a downloaded tla2tools.jar
java -cp $JAR tlc2.TLC -workers auto -deadlock -config FaultIoPR.cfg FaultIo.tla   # in a scratch copy
python3 witness.py $JAR
python3 mutants.py $JAR
cargo test -p rift-mock-core --test faultio_interleavings                          # the engine half
cargo test -p rift-mock-core --test faultio_interleavings -- --ignored             # the filed defects
```

`-deadlock` turns the deadlock check off: a kept-alive connection with nothing more to do is a legal end.

## Can a fault armed for response N+1 trip on the bytes of response N?

**Not while those bytes are in the process.** The fault can only be armed once request N+1 has been dispatched, and
hyper does not read the head of N+1 until N has left its write buffer:

1. `can_read_head` needs `Reading::Init` (`hyper-1.12.0/src/proto/h1/conn.rs:180`). A finished request leaves
   `Reading::KeepAlive`.
2. Only `try_keep_alive` moves `(KeepAlive, KeepAlive)` to `(Init, Init)` (`conn.rs:1100`), and `Conn::poll_flush` calls it
   only after `Buffered::poll_flush` was Ready (`conn.rs:855`), which writes the whole buffer and flushes the stream
   (`io.rs:278`). Over TLS that flush is tokio-rustls's, which drains the session to `FaultIo`.
3. The one exception is `pipeline_flush`, which answers Ready with the bytes still buffered (`io.rs:279`). Rift never sets
   it: no `pipeline_flush` in `crates/`, and hyper's default is off (`server/conn/http1.rs:256`).

A second path reaches `try_keep_alive`: a request body reaching EOF (`conn.rs:426`), if a response was already buffered.
Rift's handler reads the body before answering (`handler.rs:1131`), and the refusals close the connection. The `disabled`
imposter's `503` (`handler.rs:1095`) is the exception, unread body and no close. See "Unconfirmed".

The engine agrees. `faultio_interleavings.rs` pipelines a 32 MiB `GET /big` and `GET /fault` in one write and stops
reading. For 600 ms the probe sees no `FaultArmed`, since hyper cannot have dispatched N+1. The client then reads, and N
arrives whole before the fault, for every kind, over http and https:

```
EMPTY_RESPONSE: N whole, then 0 of 0 payload bytes
MALFORMED_RESPONSE_CHUNK: N whole, then 77 of 77 payload bytes
RANDOM_DATA_THEN_CLOSE: N whole, then 10 of 10 payload bytes
```

**But a reset can still destroy bytes already handed to the kernel.** That is accepted as what a reset means (#1783).

HTTP/2 does not use this ordering. `apply_tcp_fault` (`rift-http-proxy/src/gateway.rs:38`) never arms the cell on h2: the
service errs and hyper resets that stream (`proto/h2/server.rs:465`). The imposter's own listener has no such check and
relies on serving a faulting imposter HTTP/1-only, decided once at accept, which is #1782.

## Actions and the code

| Action | Code |
|:--|:--|
| `AddFaultStub` | a stub write under a live connection (`manager.rs:2844`); `http1_only` is not re-evaluated (`manager.rs:1971`) |
| `ClientSend`, `ClientWriteBody`, `ClientCloses` | the client; EOF on an idle connection closes both sides (`dispatch.rs:343`, `conn.rs:548`) |
| `ReadHead` | `can_read_head` (`conn.rs:180`) and `poll_ready` (`dispatch.rs:634`) |
| `ServiceDone` | the service future completes: `fault_cell.arm` (`manager.rs:209`), `arm_linger_if_requested` (`manager.rs:211`, `fault_io.rs:263`) |
| `WriteHead` | `poll_msg`, `write_head` (`dispatch.rs:364`, `conn.rs:602`): into the buffer, no I/O |
| `DropBody` | body receiver gone, hyper stops reading (`dispatch.rs:239`, `conn.rs:876`) |
| `HttpWrite` | `FaultIo::poll_write` (`fault_io.rs:448`), Flatten strategy (`io.rs:318`) |
| `TlsTakePayload` | `TlsPayloadFault::poll_fault_on_write`, `take_payload` (`fault_io.rs:582`, `:225`) |
| `TlsSeal`, `TlsRecordWrite` | pass-through into rustls (`fault_io.rs:617`), records into `FaultIo` (`fault_io.rs:448`) |
| `TlsPayloadSeal`, `TlsTripped` | `Trip::Writing`, `Flushing`, `Tripped` (`fault_io.rs:532`, `:553`, `:563`) |
| `FlushDone` | `Conn::poll_flush` then `try_keep_alive` (`conn.rs:855`, `:1100`) |
| `PlainShutdown`, `LingerShutWrite` | `is_done`, `Buffered::poll_shutdown` flushes first, `FaultIo::poll_shutdown` (`dispatch.rs:469`, `io.rs:337`, `fault_io.rs:458`, `:469`) |
| `DrainRead`, `DrainEnds` | `Drain::poll` (`fault_io.rs:363`) |
| `Close` | the stream is dropped; the kernel sends RST under `SO_LINGER(0)` (`fault_io.rs:408`) or with unread data |
| `Deliver` | the kernel delivers its queue in order |
| `H2Dispatch`, `H2ServiceDone`, `H2Write` | streams on one socket and one cell; the arm has no version check (`manager.rs:208`) |

## What it checks

| Invariant | Meaning | Cfg |
|:--|:--|:--|
| `FaultFiresAtMostOnce` | One armed fault, one firing | PR, full |
| `PayloadAboveTlsOnHttps` | On https the first layer to see a payload kind is the one above TLS (#1600) | PR, full |
| `FaultBeforeResponseBytes` | No byte of a fault's carrier reaches the kernel | PR, full |
| `FaultNeverTripsEarlierBytes` | A fault never takes the connection with an earlier response's bytes in the process | PR, full |
| `LingerDeliversResponse` | A reset under a refusal comes after the refusal and its FIN (#1460) | PR, full |
| `H2FaultStaysOnStream` | One h2 stream's fault takes no other stream | Known (#1782) |
| `RstDiscardsNothing` | A close that sends RST discards nothing the kernel still holds | Violated by design, witness (#1783) |

## Run table

TLC 2026.11.02, 10 workers, Apple silicon, 2026-11-03. Wall times vary with what else the machine runs.

| Cfg | Bounds | Result | Generated | Distinct | Depth | Time |
|:--|:--|:--|:--:|:--:|:--:|:--:|
| `FaultIoPR.cfg` | 2 requests, 1 body unit | no error | 88,930 | 42,894 | 30 | 2 s |
| `FaultIo.cfg` | 3 requests, 2 body units | no error | 1,708,808 | 721,960 | 43 | 8 s |
| `Known.cfg` | as PR | `H2FaultStaysOnStream` violated, depth 8 | | | | under 2 s |

`-coverage 1` on `FaultIoPR.cfg`: all 25 actions hit, the least being `TlsPayloadSeal` at 99 distinct states. The first
run showed `PlainShutdown` at zero: nothing closed a connection except a refusal, so the model gained `ClientCloses`.

## Witnesses

`witness.py` requires exactly these verdicts:

| Case | Expected |
|:--|:--|
| Two requests | `NeverArmAfterFirstWrite` violated: a fault is armed after an earlier response went out, which is why the ordering needs proving |
| One request (control) | no error |
| `PipelineFlush = TRUE` | `FaultNeverTripsEarlierBytes` violated: the gate above is the whole answer |
| `PipelineFlush = FALSE` (control) | no error |
| Plain `take` as a read | no error: an equivalent mutant, since hyper never writes after the first error (`dispatch.rs:451`, `:128`). The read matters above TLS, see below |
| Bytes pipelined past a graceful fault | `GracefulFaultEndsInFin` violated |
| A full socket under a payload fault over http | `PayloadAlwaysWritten` violated |
| `DrainOutrunsDelivery = TRUE` | `LingerDeliversResponse` violated |
| `ImposterArmsOnH2 = FALSE`, `apply_tcp_fault`'s rule (control for #1782) | no error |
| `Known.cfg` | `H2FaultStaysOnStream` violated |
| `witness.py` | `RstDiscardsNothing` violated, by design |
| `FaultIoPR.cfg` (control) | no error |

## Mutants

`mutants.py` puts each bug in, requires its invariant to report it, and then requires every other invariant to pass
with that one removed, so each is caught by its invariant only:

| Bug | Caught by |
|:--|:--|
| The payload is taken beneath TLS (#1600) | `PayloadAboveTlsOnHttps` |
| The linger drains without shutting the write side | `LingerDeliversResponse` |
| A refusal does not ask for a lingering close (#1460) | `LingerDeliversResponse` |
| `take_payload` reads instead of taking: `FaultIo` beneath fires the fault again | `FaultFiresAtMostOnce` |
| The write goes out before the fault | `FaultBeforeResponseBytes` |
| hyper reads the next head with the last response unflushed (http, no refusal) | `FaultNeverTripsEarlierBytes` |

The last one is restricted because, with TLS or a refusal in the script, it also violates `PayloadAboveTlsOnHttps` or
`LingerDeliversResponse`: earlier records in rustls make `FaultIo` the first layer to see a payload kind, and a fault
ahead of a refusal resets it away. Both reports are right, and the restriction is only there to show the invariant on
its own.

## Findings

| Issue | What | How it was verified |
|:--|:--|:--|
| #1782 | A fault stub added under an open h2 connection aborts every stream on it | Reproduced: `a_fault_stub_added_under_an_open_h2_connection_resets_only_its_stream` (`#[ignore]`) fails with the other stream at `BrokenPipe`, probe `armed [Empty], taken [(Empty, RawSocket)]` |

It is in `Known.cfg`; its fix moves the invariant to `FaultIoPR.cfg` and `FaultIo.cfg`, removes the `#[ignore]` and
deletes its row in `witness.py`.

## Documented limits, reproduced or read

- **A reset discards what the kernel still holds.** With a slow reader, a reset for pipelined N+1 delivered 33,385,511 of
  N's 33,554,432 bytes on macOS loopback. The owner accepted this as what a reset means (#1783, closed), and
  `docs/features/fault-injection.md` says so.
- **Unread bytes turn a graceful close into a reset.** Reproduced: `unread_pipelined_bytes_turn_an_empty_close_into_a_reset`
  gets `ConnectionReset` from `EMPTY_RESPONSE` with 2 MiB pipelined behind the request. The kernel decides that, and the
  class is noted on #1783 and in `docs/features/fault-injection.md`.
- **A payload over http is best-effort.** `FaultIo::trip` uses `try_write` and logs a failure (`fault_io.rs:419`), as its
  comment says. `PayloadAlwaysWritten` shows the model reaches it. The pipelined run still delivered every byte (above),
  so it is **read from the source, not reproduced**. `Trip` above TLS waits instead.

## Unconfirmed, not filed

- **The drain's bound beating delivery.** With `DrainOutrunsDelivery`, a client that has stopped reading while it keeps
  writing for over 2 s can have the refusal reset away. Bounded by design (`fault_io.rs`, `arm_linger`). Not reproduced;
  it would need a stalled reader writing throughout the deadline.
- **The `disabled` imposter's `503`.** It answers with the body unread and no close (`handler.rs:1095`), so the body's
  EOF can reach `try_keep_alive` with the `503` unflushed (`conn.rs:426`), and a fault for the next request could trip on
  it. That needs a pipelined body, a full socket for a small response, and the imposter re-enabled in between. Traced
  through the source only, and not modelled.

## Refuted

- **"A fault for N+1 trips on the bytes of N in hyper's buffer."** The question #1770 asked. Refuted by hyper's source and
  by the pipelined run, above.
- **A lingering close losing a refusal behind a blocked response.** `is_done` can be true with the refusal unflushed, which
  looked like a FIN ahead of the answer. `Buffered::poll_shutdown` flushes first (`io.rs:337`), and
  `a_refusal_behind_a_blocked_response_still_arrives_before_the_fin` passes.

## What is outside the model

- **Time.** The drain's timers can fire at any point, but only with the send queue empty unless `DrainOutrunsDelivery`.
- **Socket capacity.** A socket that cannot take a byte is an action that has not run yet.
- **CONNECT tunnels.** They share the connection's cell (`manager.rs:365`). hyper serves one layer at a time, and the
  depth adds layers without adding writers, so they are not modelled. `TlsPayloadFault` on the innermost session is
  tested by #1600's own tests.
- **The other listeners.** Admin and front door take `apply_tcp_fault`'s rule, which the control case models; their tests
  are `issue_1234_gateway_tcp_fault.rs`. The intercept listener arms only the linger.
- **h2 frames through TLS layers.** `H2Write` goes straight to the cell, so `PayloadAboveTlsOnHttps` says nothing about h2.
- **Trace validation.** The probe points (`FaultArmed`, `FaultTaken`, `LingerArmed`, `LingerFinSent`, `LingerDrained`) are
  used by assertions in `faultio_interleavings.rs`. There is no `TraceValidation.tla` for this model. `FaultCell` fires
  them only where a fault or a linger is armed or taken, so an unarmed connection never reads the probe, and only the
  imposter listener installs one (`FaultCell::probed`).
