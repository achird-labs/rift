"""The README's scene: a request meets an imposter, the first matching stub answers from its cycle,
and a proxy records an upstream answer into a stub that later requests replay.

Depicts:
- docs/mountebank/imposters.md "Stub Matching Behavior / First-Match-Wins": stubs are checked in
  order and the first whose predicates match answers.
- docs/mountebank/responses.md "Response Cycling": a stub with several responses answers with the
  next one each time (here v1, then v2; 503 is next).
- docs/mountebank/proxy.md "Proxy Modes" / "Where recordings go" and
  crates/rift-mock-core/src/imposter/core/proxy.rs `insert_or_append_proxy_stub`: a proxyOnce proxy
  (also what an omitted mode means, #1314) with `predicateGenerators` on the path forwards the
  first GET /users/9, records the answer as a `deepEquals` stub *in front of* the proxy stub, and
  the next GET /users/9 is answered by that recording without calling upstream.
Probed on a debug build of master: proxyOnce three GET /x -> U1 U1 U1 with the recording placed
before the proxy stub; a three-response stub cycles 1, 2, 3, 1.
"""

from hud import CYAN, DIM, FAINT, GREEN, INDIGO, RED, TEXT, VIOLET, Scene

T = 17.0
ROW_Y = [146, 198, 250, 302]
PANEL_X, PANEL_W = 222, 500
BADGE_X = 268
RESP_X = 470
SLOTS = [("200 v1", 498), ("200 v2", 576), ("503", 648)]
CLIENT_X = 118
UP_X = 868
LOG_Y = 384


def build() -> Scene:
    sc = Scene(
        1000, 520, T,
        "rift · programmable HTTP(S) mocks",
        "Mountebank-compatible · imposters · stubs · predicates · proxies",
        "first matching stub answers · responses cycle · a proxy records what upstream said, then replays it",
        "Rift, a Mountebank-compatible HTTP and HTTPS mock server. Requests reach an imposter and are checked "
        "against its stubs in order; the first stub whose predicates match answers with the next response in its "
        "cycle, 200 v1 and then 200 v2. A request no hand-written stub matches reaches a proxyOnce proxy stub, "
        "which forwards it upstream and records the answer as a new stub in front of itself; the next identical "
        "request is replayed from that recording without calling upstream.",
        still=T - 0.5,
    )
    sc.add(sc.text(28, 86, "CLIENT", "tiny"), sc.text(PANEL_X, 86, "IMPOSTER :4545 · stubs in order", "tiny"),
           sc.text(UP_X, 86, "UPSTREAM", "tiny", anchor="middle"))
    sc.add(sc.panel(PANEL_X - 8, 104, PANEL_W + 16, 230))
    # rows that do not move
    sc.add(sc.text(PANEL_X + 6, ROW_Y[0] + 4, "#0", "big", fill=DIM), sc.badge(BADGE_X, ROW_Y[0], "equals path=/health", DIM),
           sc.text(RESP_X, ROW_Y[0] + 4, "200 ok", "lbl", fill=DIM))
    sc.add(sc.text(PANEL_X + 6, ROW_Y[1] + 4, "#1", "big", fill=DIM),
           sc.badge(BADGE_X, ROW_Y[1], "startsWith path=/orders", DIM))
    for label, x in SLOTS:
        sc.add(sc.box(x - 32, ROW_Y[1] - 12, 64, 24, INDIGO, ".08", rx=4),
               sc.text(x, ROW_Y[1] + 4, label, "cap", fill=TEXT, anchor="middle"))
    # the cycle's next-response marker
    nxt = sc.path([(0.0, 0, 0), (2.3, 0, 0), (2.6, SLOTS[1][1] - SLOTS[0][1], 0), (5.9, SLOTS[1][1] - SLOTS[0][1], 0),
                   (6.2, SLOTS[2][1] - SLOTS[0][1], 0), (T - 0.3, SLOTS[2][1] - SLOTS[0][1], 0), (T, 0, 0)])
    sc.add(sc.g(nxt, f'<path d="M{SLOTS[0][1] - 6} {ROW_Y[1] - 20} h12 l-6 6z" fill="{CYAN}"/>'))

    # the proxy stub, pushed down by the recording put in front of it
    rec_at = 11.1
    sc.add(sc.g(sc.path([(0.0, 0, 0), (rec_at, 0, 0), (rec_at + 0.5, 0, ROW_Y[3] - ROW_Y[2]), (T - 0.3, 0, ROW_Y[3] - ROW_Y[2]),
                         (T, 0, 0)]),
                sc.g(sc.shown([(0.0, rec_at), (T - 0.3, T)], 0.1), sc.text(PANEL_X + 6, ROW_Y[2] + 4, "#2", "big", fill=DIM)),
                sc.badge(BADGE_X, ROW_Y[2], "(no predicates)", DIM),
                sc.text(RESP_X, ROW_Y[2] + 4, "proxy → upstream · proxyOnce", "lbl", fill=VIOLET)))
    sc.add(sc.g(sc.shown([(rec_at, T - 0.3)]),
                sc.text(PANEL_X + 6, ROW_Y[2] + 4, "#2", "big", fill=GREEN),
                sc.badge(BADGE_X, ROW_Y[2], "deepEquals path=/users/9", GREEN),
                sc.text(RESP_X + 72, ROW_Y[2] + 4, "200 user 9 · recorded", "lbl", fill=GREEN)))
    sc.add(sc.g(sc.shown([(rec_at + 0.4, T - 0.3)]), sc.text(PANEL_X + 6, ROW_Y[3] + 4, "#3", "big", fill=DIM)))

    # upstream
    sc.add(sc.box(UP_X - 84, ROW_Y[2] - 24, 168, 48, DIM, ".08"),
           sc.text(UP_X, ROW_Y[2] + 4, "api.upstream", "lbl", anchor="middle"),
           sc.rail(PANEL_X + PANEL_W + 10, ROW_Y[2], UP_X - 86, ROW_Y[2], VIOLET))

    sc.add(sc.text(PANEL_X, LOG_Y - 18, "CLIENT RECEIVED", "tiny"))

    # (start, path, [(y, badge text, ok)], response, response origin x, answer y, log text)
    reqs = [
        (0.4, "GET /orders/7", [(0, "equals path=/health", False), (1, "startsWith path=/orders", True)],
         "200 v1", SLOTS[0][1], 1, "stub #1, response 1 of its cycle"),
        (3.9, "GET /orders/7", [(0, "equals path=/health", False), (1, "startsWith path=/orders", True)],
         "200 v2", SLOTS[1][1], 1, "stub #1, response 2: the cycle moved on"),
        (7.4, "GET /users/9", [(0, "equals path=/health", False), (1, "startsWith path=/orders", False),
                               (2, "(no predicates)", True)],
         "200 user 9", RESP_X + 60, 2, "proxied upstream, recorded in front of the proxy"),
        (12.6, "GET /users/9", [(0, "equals path=/health", False), (1, "startsWith path=/orders", False),
                                (2, "deepEquals path=/users/9", True)],
         "200 user 9", RESP_X + 100, 2, "replayed by the recording; upstream not called"),
    ]
    for n, (s, path, checks, resp, ox, ans, note) in enumerate(reqs):
        proxied = n == 2
        color = VIOLET if n >= 2 else CYAN
        # request chip and scan cursor follow each other down the rows
        t = s + 0.5
        cpts, rpts = [(s, -40, 0, 0), (s + 0.3, 0, 0)], [(s + 0.3, 0, 0, 0), (s + 0.5, 0, 0)]
        for row, text, ok in checks:
            dy = ROW_Y[row] - ROW_Y[0]
            cpts += [(t, 0, dy), (t + 0.45, 0, dy)]
            rpts += [(t, 0, dy), (t + 0.45, 0, dy)]
            mark, c = ("✓", GREEN) if ok else ("✗", RED)
            sc.add(sc.g(sc.shown([(t + 0.15, s + 3.3 if not proxied else rec_at)], 0.1),
                        sc.badge(BADGE_X, ROW_Y[row], f"{text} {mark}", c)))
            t += 0.5
        done = t + (2.0 if proxied else 0.2)
        cpts += [(done, 0, ROW_Y[ans] - ROW_Y[0]), (done + 0.2, 0, ROW_Y[ans] - ROW_Y[0], 0)]
        rpts += [(done, 0, ROW_Y[ans] - ROW_Y[0]), (done + 0.2, 0, ROW_Y[ans] - ROW_Y[0], 0)]
        sc.add(sc.g(sc.path(cpts, "linear"), sc.chip(CLIENT_X, ROW_Y[0], path, color)))
        sc.add(sc.g(sc.path(rpts, "linear"),
                    f'<rect x="{PANEL_X - 4}" y="{ROW_Y[0] - 22}" width="{PANEL_W + 8}" height="44" rx="7" fill="none" '
                    f'stroke="{color}" stroke-width="1.6" filter="url(#glow)"/>'))
        y = ROW_Y[ans]
        if proxied:
            out = t
            sc.add(sc.g(sc.path([(out, 0, 0, 0), (out + 0.1, 0, 0), (out + 0.7, UP_X - 100 - (PANEL_X + PANEL_W + 40), 0),
                                 (out + 0.75, UP_X - 100 - (PANEL_X + PANEL_W + 40), 0, 0)]),
                        sc.chip(PANEL_X + PANEL_W + 40, y, "GET", VIOLET)))
            sc.add(sc.pulse(UP_X, y, VIOLET, out + 0.75, 20))
            sc.add(sc.g(sc.shown([(out + 0.75, T - 0.3)]),
                        sc.text(UP_X, ROW_Y[2] - 34, "calls: 1", "cap", fill=VIOLET, anchor="middle")))
            back = out + 0.9
            rpath = [(back, UP_X - 90 - CLIENT_X, 0, 0), (back + 0.1, UP_X - 90 - CLIENT_X, 0),
                     (back + 0.7, ox - CLIENT_X, 0), (rec_at + 0.3, ox - CLIENT_X, 0)]
            leave = rec_at + 0.3
        else:
            leave = t + 0.2
            rpath = [(leave - 0.1, ox - CLIENT_X, 0, 0), (leave, ox - CLIENT_X, 0)]
            if n == 3:
                sc.add(sc.g(sc.shown([(leave, T - 0.3)]),
                            sc.text(UP_X, ROW_Y[2] + 44, "not called · replayed", "cap", fill=GREEN, anchor="middle")))
        arrive = leave + 0.8
        ly = LOG_Y + n * 20
        rpath += [(arrive, 0, 0), (arrive + 0.3, 0, ly - y - 4), (arrive + 0.4, 0, ly - y - 4, 0)]
        sc.add(sc.g(sc.path(rpath), sc.chip(CLIENT_X, y, resp, GREEN)))
        sc.add(sc.g(sc.shown([(arrive + 0.3, T - 0.2)], 0.15),
                    sc.text(PANEL_X, ly, f"{path:<14}→ {resp:<11}", "lbl", fill=GREEN),
                    sc.text(PANEL_X + 230, ly, note, "cap")))
    return sc
