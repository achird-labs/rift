"""One connection through the engine, from accept to response, with `--local-only` refusing a remote peer.

Depicts crates/rift-mock-core/src/imposter/manager.rs `run_accept_loop`, in its order:
- accept on the imposter's listener;
- the `--local-only` peer check (#1523): a non-loopback peer is dropped right after accept, before
  tuning, FaultIo and TLS, which is Mountebank's `socket.destroy()` (PeerPolicy::LoopbackOnly,
  `is_loopback` on the canonical address; docs/configuration/cli.md `--local-only`);
- socket tuning: `apply_stream_tuning` sets TCP_NODELAY;
- FaultIo wraps the stream *beneath* TLS: a transport fault is armed by the handler and applied by
  FaultIo on the response write, so it also breaks an HTTPS connection
  (docs/features/fault-injection.md "TCP Faults", "Top-Level Fault Response");
- TLS for https imposters, with a 10 s handshake bound; ALPN offers h2 and http/1.1;
- HTTP/1 or h2 (h2c by preface on plain HTTP; HTTP/1 only when the imposter uses TCP faults or
  scripts) - docs/mountebank/imposters.md "HTTP/2 and h2c";
- the first matching stub, then its behaviors in Mountebank's order wait, lookup, copy,
  shellTransform, decorate (docs/mountebank/behaviors.md "Behavior Order"; an array form runs in
  its own order), then the response is written back.
Simplified: the connection-cap permit taken before accept, the accept-error backoff and the
handshake timeout are left out. The peer check was verified from source and its tests; the debug
build available predates #1523, so it was not probed.
"""

from hud import AMBER, CYAN, DIM, FAINT, GREEN, INDIGO, RED, TEXT, VIOLET, Scene

T = 12.5
ROW1_Y, ROW2_Y, RAIL_Y = 206, 340, 438
BOX_W, BOX_H = 116, 74
X0, GAP = 40, 16
STAGES = [
    ("accept", ["imposter port", "listener"]),
    ("peer check", ["--local-only", "loopback only"]),
    ("socket tuning", ["TCP_NODELAY"]),
    ("FaultIo", ["wraps stream", "under TLS"]),
    ("TLS", ["https only", "ALPN h2/http1.1"]),
    ("HTTP/1 · h2", ["h2c by preface", "on plain http"]),
    ("match", ["first stub whose", "predicates match"]),
]
CX = [X0 + BOX_W / 2 + i * (BOX_W + GAP) for i in range(len(STAGES))]
BEHAVIORS = [("wait", 404), ("lookup", 512), ("copy", 620), ("shellTransform", 752), ("decorate", 890)]
FAULT_X = CX[3]


def build() -> Scene:
    sc = Scene(
        1000, 560, T,
        "connection path · accept to response",
        "rift --local-only · https imposter :4545",
        "--local-only drops a non-loopback peer at accept, before tuning, FaultIo and TLS; faults fire on the write",
        "One connection through a Rift imposter started with --local-only. A remote client, 203.0.113.7, is "
        "accepted and dropped at the peer check. A loopback client, 127.0.0.1, passes the peer check, socket "
        "tuning (TCP_NODELAY), FaultIo, the TLS handshake of an https imposter and HTTP/1 or h2, matches a stub, "
        "and runs its behaviors in order: wait, lookup, copy, shellTransform, decorate. The response is written "
        "back through FaultIo, where a fault armed by the stub would reset or cut the connection.",
        still=T - 0.4,
    )
    sc.add(sc.text(28, 86, "CLIENTS", "tiny"), sc.text(972, 86, "ENGINE · one task per connection", "tiny", anchor="end"))
    for i, ((name, sub), x) in enumerate(zip(STAGES, CX)):
        c = AMBER if name == "peer check" else (VIOLET if name == "FaultIo" else INDIGO)
        sc.add(sc.box(x - BOX_W / 2, ROW1_Y - BOX_H / 2, BOX_W, BOX_H, c, ".08"),
               sc.text(x, ROW1_Y - 12, name, "big", fill=TEXT, anchor="middle"))
        for k, s in enumerate(sub):
            sc.add(sc.text(x, ROW1_Y + 8 + k * 15, s, "cap", anchor="middle"))
        if i:
            sc.add(sc.arrow(CX[i - 1] + BOX_W / 2 + 1, ROW1_Y, x - BOX_W / 2 - 2, ROW1_Y, FAINT))
    # Behaviors, in Mountebank's order
    sc.add(sc.text(BEHAVIORS[0][1] - 50, ROW2_Y - 44, "BEHAVIORS · Mountebank order (an array runs in its own order)",
                   "tiny"))
    for i, (name, x) in enumerate(BEHAVIORS):
        w = 120 if name == "shellTransform" else 92
        sc.add(sc.box(x - w / 2, ROW2_Y - 22, w, 44, CYAN, ".07"), sc.text(x, ROW2_Y + 4, name, "lbl", anchor="middle"))
        if i:
            pw = 120 if BEHAVIORS[i - 1][0] == "shellTransform" else 92
            sc.add(sc.arrow(BEHAVIORS[i - 1][1] + pw / 2 + 1, ROW2_Y, x - w / 2 - 2, ROW2_Y, FAINT))
    sc.add(sc.arrow(CX[6], ROW1_Y + BOX_H / 2 + 2, CX[6], ROW2_Y - 24, FAINT))
    # The return path: the response is written back through FaultIo
    sc.add(sc.rail(BEHAVIORS[-1][1], RAIL_Y, 40, RAIL_Y, GREEN),
           sc.line(BEHAVIORS[-1][1], ROW2_Y + 22, BEHAVIORS[-1][1], RAIL_Y, FAINT),
           sc.text(500, RAIL_Y - 10, "response written back: HTTP -> TLS -> FaultIo -> socket", "cap"),
           sc.box(FAULT_X - 6, RAIL_Y - 8, 12, 16, VIOLET, ".3", rx=3),
           sc.text(FAULT_X, RAIL_Y + 24, "a fault armed by the matched stub fires here, on the write:", "cap",
                   fill=VIOLET, anchor="middle"),
           sc.text(FAULT_X, RAIL_Y + 40, "reset · empty · random data · malformed chunk", "cap", fill=VIOLET,
                   anchor="middle"))

    # The remote client: accepted, then dropped at the peer check
    rx0, ry0 = 150, 112
    sc.add(sc.chip(rx0, ry0, "203.0.113.7 · remote", DIM, glow=False),
           sc.chip(410, ry0, "127.0.0.1 · loopback", DIM, glow=False))
    sc.add(sc.g(sc.path([(0.3, 0, 0, 0), (0.5, 0, 0), (1.1, CX[0] - rx0, ROW1_Y - 52 - ry0), (1.5, CX[0] - rx0, ROW1_Y - 52 - ry0),
                         (2.0, CX[1] - rx0, ROW1_Y - 52 - ry0), (2.6, CX[1] - rx0, ROW1_Y - 52 - ry0),
                         (3.0, CX[1] - rx0, ROW1_Y - 52 - ry0, 0)]),
                sc.chip(rx0, ry0, "conn", RED)))
    sc.add(sc.pulse(CX[1], ROW1_Y, RED, 2.0, 20),
           sc.g(sc.shown([(2.1, T)]), sc.text(CX[1], ROW1_Y + BOX_H / 2 + 18, "✗ remote: dropped", "mark", fill=RED,
                                               anchor="middle")))

    # The loopback client: all the way through
    lx0 = 410
    up = ROW1_Y - 52 - ry0
    pts = [(2.6, 0, 0, 0), (2.8, 0, 0), (3.4, CX[0] - lx0, up)]
    t = 3.4
    for x in CX[1:]:
        pts += [(t + 0.2, pts[-1][1], up), (t + 0.6, x - lx0, up)]
        t += 0.6
    stamp = {1: "✓ loopback: admitted", 4: "handshake", 5: "h2"}
    for i, x in enumerate(CX):
        if i in stamp:
            ti = 3.4 + 0.6 * i
            sc.add(sc.g(sc.shown([(ti, T)]), sc.text(x, ROW1_Y + BOX_H / 2 + 18 + (18 if i == 1 else 0), stamp[i],
                                                     "mark", fill=GREEN, anchor="middle")))
    # down to the behaviors, through them, then back along the rail
    down = ROW2_Y - 38 - ry0
    pts += [(t + 0.2, CX[6] - lx0, up), (t + 0.6, CX[6] - lx0, down), (t + 0.8, BEHAVIORS[0][1] - lx0, down)]
    t += 0.8
    arrivals = [t]
    for _, x in BEHAVIORS[1:]:
        pts += [(t + 0.15, pts[-1][1], down), (t + 0.45, x - lx0, down)]
        t += 0.45
        arrivals.append(t)
    sc.add(sc.g(sc.path(pts + [(t + 0.15, pts[-1][1], down, 0)]), sc.chip(lx0, ry0, "conn", CYAN)))
    for (_, x), at in zip(BEHAVIORS, arrivals):
        sc.add(sc.pulse(x, ROW2_Y, CYAN, at, 14))
    # the response
    r0 = t + 0.2
    rpts = [(r0, 0, 0, 0), (r0 + 0.1, 0, 0), (r0 + 0.8, FAULT_X - BEHAVIORS[-1][1], 0),
            (r0 + 1.1, FAULT_X - BEHAVIORS[-1][1], 0), (r0 + 1.6, 60 - BEHAVIORS[-1][1], 0)]
    rpts += [(T, rpts[-1][1], 0)]
    sc.add(sc.g(sc.path(rpts), sc.chip(BEHAVIORS[-1][1], RAIL_Y, "200", GREEN)))
    sc.add(sc.g(sc.shown([(r0 + 1.6, T)]), sc.text(40, RAIL_Y + 24, "→ 127.0.0.1", "cap", fill=GREEN)))
    return sc
