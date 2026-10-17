"""Proxy modes: what proxyOnce, proxyAlways and proxyTransparent record, where, and what replays.

Depicts, for a proxy stub with `predicateGenerators: [{"matches": {"path": true}}]`:
- docs/mountebank/proxy.md "Proxy Modes" and "Where recordings go", and
  crates/rift-mock-core/src/imposter/core/proxy.rs `insert_or_append_proxy_stub` /
  `placement_for_mode`:
  - proxyOnce: the first request is forwarded and recorded as a new stub *in front of* the proxy
    stub, so the next matching request is answered by the recording and never reaches upstream;
  - proxyAlways: every request is forwarded; the first recording becomes a stub *after* the proxy
    (the end of the list), and later ones append their responses to that stub (equal predicates).
    The proxy still matches first, so the recording answers only once the proxy stops matching
    (`mb replay`, or the proxy stub removed);
  - proxyTransparent: every request is forwarded and nothing is recorded.
- A recorded predicate defaults to `deepEquals` (docs/mountebank/proxy.md "What Gets Recorded").
- An omitted `mode` is proxyOnce, as in Mountebank (#1314, `ProxyMode::parse(..).unwrap_or_default()`).
Probed on a debug build of master, three GET /x then GET /y per mode against a cycling upstream:
  proxyOnce        U1 U1 U1, stubs [deepEquals /x [U1], deepEquals /y [U2], proxy]
  proxyAlways      U3 U4 U5, stubs [proxy, deepEquals /x [U3 U4 U5], deepEquals /y [U6]]
  proxyTransparent U7 U8 U1, stubs [proxy]
  (mode omitted)   same as proxyOnce.
Simplified: each lane has its own upstream, numbered from U1.
"""

from hud import CYAN, DIM, FAINT, GREEN, INDIGO, TEXT, VIOLET, Scene

T = 13.0
STARTS = [0.6, 4.6, 8.6]
LANE_Y = [104, 244, 384]
LANE_H = 128
CLIENT_X = 252
LIST_X0, LIST_X1 = 316, 664
UP_X = 812
ROW_DY = [44, 80]  # row centres inside a lane
RESP_X = [520, 550, 580]  # the recorded stub's response badges

LANES = [
    ("proxyOnce", CYAN, ["also the default when", "mode is omitted (#1314)"], "once"),
    ("proxyAlways", INDIGO, ["forwards every request;", "records behind the proxy"], "always"),
    ("proxyTransparent", VIOLET, ["forwards every request;", "records nothing"], "transparent"),
]


def lane(sc: Scene, top: float, name: str, color: str, notes: list[str], kind: str) -> None:
    sc.add(sc.panel(20, top, 960, LANE_H, "#0d0e1c", "#22244a"))
    sc.add(sc.text(36, top + 32, name, "big", fill=color))
    for i, s in enumerate(notes):
        sc.add(sc.text(36, top + 54 + i * 17, s, "cap"))
    sc.add(sc.text(CLIENT_X, top + 22, "client", "tiny", anchor="middle"),
           sc.text(LIST_X0, top + 22, "imposter stubs, in order", "tiny"),
           sc.text(UP_X, top + 22, "upstream", "tiny", anchor="middle"))
    sc.add(sc.panel(LIST_X0 - 6, top + 26, LIST_X1 - LIST_X0 + 12, 74, "#14152a"))
    sc.add(sc.box(UP_X - 56, top + 30, 112, 36, DIM, ".08"), sc.text(UP_X, top + 53, "api.upstream", "cap",
                                                                      anchor="middle"))
    sc.add(sc.rail(LIST_X1 + 6, top + 48, UP_X - 58, top + 48, color))
    sc.add(sc.text(36, top + 112, "got:", "cap"))

    row = lambda i: top + ROW_DY[i]  # noqa: E731
    rec_at = STARTS[0] + 2.0  # the first recording lands here

    # The proxy stub; under proxyOnce it slides down when the recording is put in front of it
    proxy_moves = [(0.0, 0, 0)]
    if kind == "once":
        proxy_moves += [(rec_at, 0, 0), (rec_at + 0.5, 0, ROW_DY[1] - ROW_DY[0]), (T - 0.4, 0, ROW_DY[1] - ROW_DY[0]),
                        (T, 0, 0)]
    sc.add(sc.g(sc.path(proxy_moves),
                sc.text(LIST_X0 + 8, row(0) + 4, "proxy → api.upstream", "lbl", fill=color),
                sc.text(LIST_X1 - 8, row(0) + 4, f"mode: {name}", "cap", anchor="end")))

    calls, got = 0, []
    for n, s in enumerate(STARTS):
        forwarded = kind != "once" or n == 0
        ans_y = row(0)  # proxyOnce: the proxy, then the recording put in front of it; otherwise the proxy
        # request: client -> the answering row
        sc.add(sc.g(sc.path([(s, 0, ans_y - (top + 76), 0), (s + 0.1, 0, ans_y - (top + 76)),
                             (s + 0.5, LIST_X0 - CLIENT_X - 30, ans_y - (top + 76)),
                             (s + 0.6, LIST_X0 - CLIENT_X - 30, ans_y - (top + 76), 0)]),
                    sc.chip(CLIENT_X, top + 76, "GET /x", TEXT, glow=False)))
        # the answering row lights up
        sc.add(sc.g(sc.shown([(s + 0.5, s + (2.4 if forwarded else 1.2))], 0.1),
                    f'<rect x="{LIST_X0 - 2}" y="{ans_y - 16}" width="{LIST_X1 - LIST_X0 + 4}" height="32" rx="6" '
                    f'fill="none" stroke="{color if forwarded else GREEN}" stroke-width="1.6"/>'))
        y_base = ans_y
        if forwarded:
            calls += 1
            u = f"U{calls}"
            sc.add(sc.g(sc.path([(s + 0.6, 0, 0, 0), (s + 0.7, 0, 0), (s + 1.2, UP_X - 70 - LIST_X1, 0),
                                 (s + 1.25, UP_X - 70 - LIST_X1, 0, 0)]),
                        sc.chip(LIST_X1 + 30, top + 48, "GET /x", color)))
            sc.add(sc.pulse(UP_X, top + 48, color, s + 1.2, 16))
            t_client = s + 2.6
            back = [(s + 1.25, UP_X - 90 - CLIENT_X, top + 48 - y_base, 0), (s + 1.3, UP_X - 90 - CLIENT_X, top + 48 - y_base),
                    (s + 1.9, LIST_X1 - 40 - CLIENT_X, 0)]
        else:
            u = "U1"
            t_client = s + 1.5
            back = [(s + 0.7, RESP_X[0] - CLIENT_X, 0, 0), (s + 0.8, RESP_X[0] - CLIENT_X, 0)]
            sc.add(sc.g(sc.shown([(s + 0.7, s + 3.2)]),
                        sc.text(UP_X, top + 101, "replayed · upstream not called", "cap", fill=GREEN, anchor="middle")))
        gx = 88 + len(got) * 30 - CLIENT_X
        back += [(t_client, gx, top + 108 - y_base), (t_client + 0.1, gx, top + 108 - y_base, 0)]
        sc.add(sc.g(sc.path(back), sc.chip(CLIENT_X, y_base, u, GREEN)))
        got.append((t_client, u))
        # what gets recorded
        if forwarded and kind == "once" and n == 0:
            rec = sc.shown([(rec_at, T - 0.4)])
            sc.add(sc.g(rec, sc.text(LIST_X0 + 8, row(0) + 4, "deepEquals {path: /x}", "lbl", fill=GREEN),
                        sc.badge(RESP_X[0] - 14, row(0), "U1", GREEN),
                        sc.text(LIST_X1 - 8, row(0) + 4, "new · ahead", "cap", fill=GREEN, anchor="end")))
        if forwarded and kind == "always":
            if n == 0:
                sc.add(sc.g(sc.shown([(rec_at, T - 0.4)]),
                            sc.text(LIST_X0 + 8, row(1) + 4, "deepEquals {path: /x}", "lbl", fill=GREEN),
                            sc.text(LIST_X1 - 8, row(1) + 4, "behind", "cap", fill=GREEN, anchor="end")))
            sc.add(sc.g(sc.shown([(s + 2.0, T - 0.4)]), sc.badge(RESP_X[n] - 14, row(1), u, GREEN)))
    # the upstream's call counter and the client's log
    count = 0
    for n, s in enumerate(STARTS):
        if kind != "once" or n == 0:
            count += 1
            end = T if n == len(STARTS) - 1 or kind == "once" else STARTS[n + 1] + 1.2
            sc.add(sc.g(sc.shown([(s + 1.2, end)], 0.05),
                        sc.text(UP_X, top + 82, f"calls: {count}", "lbl", fill=color, anchor="middle")))
    for i, (t, u) in enumerate(got):
        sc.add(sc.g(sc.shown([(t, T - 0.2)], 0.1), sc.text(76 + i * 30, top + 112, u, "big", fill=GREEN)))
    if kind == "always":
        sc.add(sc.text(LIST_X0, top + 118, "the proxy still matches first: the recording answers once it stops (mb replay)", "cap", fill=FAINT))
    if kind == "transparent":
        sc.add(sc.text(LIST_X0, top + 118, "a plain reverse proxy: the stub list never changes", "cap", fill=FAINT))
    if kind == "once":
        sc.add(sc.text(LIST_X0, top + 118, "the recording sits in front of the proxy, so it answers the next GET /x",
                       "cap", fill=FAINT))


def build() -> Scene:
    sc = Scene(
        1000, 580, T,
        "proxy modes · record, place, replay",
        "predicateGenerators: [{matches: {path: true}}]",
        "without predicateGenerators, proxyOnce replays from an internal store instead of a stub",
        "Rift's three proxy modes, each sent GET /x three times. proxyOnce, also the default when mode is omitted, "
        "forwards the first request, records the answer as a new stub in front of the proxy stub, and replays it, "
        "so upstream is called once. proxyAlways forwards every request and records behind the proxy, appending "
        "each answer to one recorded stub that answers only once the proxy stops matching. proxyTransparent "
        "forwards every request and records nothing.",
        still=T - 0.6,
    )
    for (name, color, notes, kind), top in zip(LANES, LANE_Y):
        lane(sc, top, name, color, notes, kind)
    return sc
