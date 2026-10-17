"""Scenarios: a state machine gates which stubs can match, and `newScenarioState` moves the token.

Depicts docs/features/scenarios.md ("Stub fields", "Arranging and inspecting state"), a Rift
extension (WireMock-style; Mountebank has no scenarios):
- a stub with `requiredScenarioState` is eligible only while its scenario is in that state; the
  implicit first state is `Started`;
- after a stub answers, `newScenarioState` (when present) moves the scenario; a stub without one
  leaves the state where it is;
- with no eligible stub the request falls through to the imposter's `defaultResponse`;
- `POST /imposters/:port/scenarios/reset` puts every scenario of a flow back to `Started`.
State is per flow id, by default the imposter port, so this one imposter has one timeline.
Probed on a debug build of master with these three stubs and `defaultResponse: {statusCode: 404}`:
POST /ship -> 404 (Started), /pay -> 200 (paid), /pay -> 409 (paid), /ship -> 202 (shipped),
then scenarios/reset -> Started.
"""

from hud import AMBER, CYAN, DIM, FAINT, GREEN, INDIGO, RED, TEXT, VIOLET, Scene

T = 15.0
NODES = {"Started": 190, "paid": 490, "shipped": 790}
NODE_Y = 156
ROW_Y = [276, 310, 344]
COL = {"#": 52, "req": 92, "pred": 300, "new": 500, "resp": 690}
STUBS = [
    ("Started", "equals path=/pay", "paid", "200 paid"),
    ("paid", "equals path=/ship", "shipped", "202 shipped"),
    ("paid", "equals path=/pay", "(none)", "409 already paid"),
]
REQ_X = 905


def build() -> Scene:
    sc = Scene(
        1000, 560, T,
        "scenarios · a state machine over stubs",
        "scenarioName: order · state per flow id (default: the port)",
        "requiredScenarioState gates a stub; newScenarioState moves the token; no eligible stub -> defaultResponse",
        "A Rift scenario with states Started, paid and shipped. Stub 0 answers POST /pay only in Started and moves "
        "the scenario to paid; stub 1 answers POST /ship only in paid and moves it to shipped; stub 2 answers "
        "POST /pay in paid and leaves the state alone. POST /ship in Started matches no eligible stub and gets the "
        "defaultResponse 404; then /pay gets 200, /pay again 409, /ship 202, and a scenarios reset puts the "
        "token back to Started.",
        still=T - 0.4,
    )
    # The state machine
    colors = {"Started": CYAN, "paid": INDIGO, "shipped": VIOLET}
    sc.add(sc.text(28, 86, "SCENARIO \"order\"", "tiny"), sc.text(28, 104, "ring = current state", "tiny", fill=GREEN))
    for name, x in NODES.items():
        c = colors[name]
        sc.add(f'<circle cx="{x}" cy="{NODE_Y}" r="30" fill="{c}" fill-opacity=".12" stroke="{c}" stroke-width="1.6"/>',
               sc.text(x, NODE_Y + 4, name, "lbl", fill=c, anchor="middle"))
    sc.add(sc.arrow(NODES["Started"] + 34, NODE_Y, NODES["paid"] - 36, NODE_Y),
           sc.text((NODES["Started"] + NODES["paid"]) / 2, NODE_Y - 8, "#0 POST /pay", "cap", anchor="middle"),
           sc.arrow(NODES["paid"] + 34, NODE_Y, NODES["shipped"] - 36, NODE_Y),
           sc.text((NODES["paid"] + NODES["shipped"]) / 2, NODE_Y - 8, "#1 POST /ship", "cap", anchor="middle"))
    px = NODES["paid"]
    sc.add(f'<path d="M{px - 16} {NODE_Y - 27} C{px - 40} {NODE_Y - 80} {px + 40} {NODE_Y - 80} {px + 16} {NODE_Y - 27}" '
           f'fill="none" stroke="{DIM}" stroke-width="1.4" marker-end="url(#tip)" style="color:{DIM}"/>',
           sc.text(px + 34, NODE_Y - 62, "#2 POST /pay · state stays", "cap"))
    sx, ex = NODES["shipped"], NODES["Started"]
    sc.add(f'<path d="M{sx} {NODE_Y + 32} C{sx - 60} {NODE_Y + 62} {ex + 60} {NODE_Y + 62} {ex} {NODE_Y + 36}" '
           f'fill="none" stroke="{AMBER}" stroke-opacity=".6" stroke-width="1.4" stroke-dasharray="4 5" '
           f'marker-end="url(#tip)" style="color:{AMBER}"/>',
           sc.text((sx + ex) / 2, NODE_Y + 66, "POST /imposters/:port/scenarios/reset", "cap", fill=AMBER,
                   anchor="middle"))

    # The stubs
    sc.add(sc.panel(28, 236, 804, 128))
    hy = 256
    for key, label in [("#", "#"), ("req", "requiredScenarioState"), ("pred", "predicate"),
                       ("new", "newScenarioState"), ("resp", "response")]:
        sc.add(sc.text(COL[key], hy, label, "tiny"))
    for i, ((req, pred, new, resp), y) in enumerate(zip(STUBS, ROW_Y)):
        sc.add(sc.text(COL["#"], y + 4, f"#{i}", "big", fill=DIM),
               sc.badge(COL["req"], y, req, colors[req]),
               sc.text(COL["pred"], y + 4, pred, "lbl", fill=DIM),
               sc.text(COL["new"], y + 4, new, "lbl", fill=colors.get(new, FAINT)),
               sc.text(COL["resp"], y + 4, resp, "lbl", fill=DIM))
    sc.add(sc.text(REQ_X, 256, "REQUEST", "tiny", anchor="middle"))
    sc.add(sc.text(28, 398, "LOG", "tiny"))

    # (start, path, state before, [(row, ok)], answer row or None, response, new state or None)
    steps = [
        (0.5, "POST /ship", "Started", [(1, False)], None, "404 default", None),
        (3.4, "POST /pay", "Started", [(0, True)], 0, "200 paid", "paid"),
        (6.2, "POST /pay", "paid", [(0, False), (2, True)], 2, "409 already paid", None),
        (9.0, "POST /ship", "paid", [(1, True)], 1, "202 shipped", "shipped"),
    ]
    token = [(0.0, 0, 0)]
    state = "Started"
    for n, (s, path, before, checks, ans, resp, new) in enumerate(steps):
        end = s + 2.7
        sc.add(sc.g(sc.shown([(s, end)]), sc.chip(REQ_X, 288, path, TEXT, glow=False)))
        t = s + 0.5
        for row, ok in checks:
            y = ROW_Y[row]
            c = GREEN if ok else RED
            note = f"state {before} ✓" if ok else f"state is {before} ✗"
            sc.add(sc.g(sc.shown([(t, end)], 0.1),
                        f'<rect x="36" y="{y - 15}" width="788" height="30" rx="6" fill="none" stroke="{c}" '
                        f'stroke-width="1.6"/>',
                        sc.badge(COL["req"], y, STUBS[row][0], c),
                        sc.text(COL["req"] + 70, y + 4, note, "cap", fill=c)))
            t += 0.4
        c = GREEN if ans is not None else INDIGO
        sc.add(sc.g(sc.shown([(t, end)]), sc.chip(REQ_X, 326, resp, c)))
        msg = f"{path:<10} in {before:<7} -> " + (f"#{ans} {resp}" if ans is not None else "no eligible stub -> "
                                                    "defaultResponse 404")
        if new:
            msg += f", state -> {new}"
        elif ans is not None:
            msg += ", state stays"
        sc.add(sc.g(sc.shown([(t + 0.2, T - 0.1)], 0.2), sc.text(28, 420 + n * 18, msg, "cap", fill=TEXT)))
        if new:
            dx = NODES[new] - NODES["Started"]
            token += [(t, NODES[state] - NODES["Started"], 0), (t + 0.7, dx, 0)]
            state = new
        else:
            k = NODES[state] - NODES["Started"]
            token += [(t, k, 0)]
    # The reset
    r = 12.0
    sc.add(sc.g(sc.shown([(r, T - 0.1)]), sc.chip(REQ_X, 288, "scenarios/reset", AMBER)))
    sc.add(sc.g(sc.shown([(r + 0.8, T - 0.1)], 0.2),
                sc.text(28, 420 + 4 * 18, "POST /imposters/:port/scenarios/reset -> state -> Started", "cap", fill=AMBER)))
    k = NODES[state] - NODES["Started"]
    token += [(r + 0.5, k, 0), (r + 0.8, k, 46), (r + 1.6, 0, 46), (r + 1.9, 0, 0)]
    sc.add(sc.g(sc.path(token),
                f'<circle cx="{NODES["Started"]}" cy="{NODE_Y}" r="38" fill="none" stroke="{GREEN}" stroke-width="2" '
                f'filter="url(#glow)"/>'))
    return sc
