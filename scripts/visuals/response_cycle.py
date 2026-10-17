"""Response cycling: a stub's responses as a rotating queue, `repeat`, and what resets the cycle.

Depicts:
- docs/mountebank/responses.md "Response Cycling" and "Cycling with Repeat Behavior": each request
  takes the response at the head; after it has been served `repeat` times (default 1) it goes to
  the tail. Probed on a debug build of master: [A repeat 2, B, C] serves A A B C A.
- docs/mountebank/responses.md "Cycling State": the cycle restarts when the stub is replaced at its
  index (`PUT /imposters/:port/stubs/:index`, #1515), for every imposter in a `PUT /imposters` body
  unless `?reconcile=true` (#1518), and on `DELETE /imposters/:port/savedProxyResponses` (#1519).
  It is kept when the stub is replaced by id (`PUT .../stubs/by-id/:id`) and when a reload leaves
  the stub unchanged. Probed: after one A, by-id PUT -> next is B (kept); index PUT -> A (reset);
  DELETE savedProxyResponses -> A; PUT /imposters -> A; PUT /imposters?reconcile=true -> kept.
- crates/rift-mock-core/src/behaviors/cycler.rs: Rift stores the queue as a (response, repeat)
  cursor over the unchanged list rather than moving responses; what a client observes is the same
  as Mountebank's rotating queue, which is what this scene draws.
"""

from hud import AMBER, CYAN, DIM, FAINT, GREEN, INDIGO, TEXT, VIOLET, Scene

T = 17.0
SLOT_X = [250, 380, 510]  # card centres; slot 0 is the head
CARD_Y, CARD_W, CARD_H = 206, 108, 66
LIFT = -72
CLIENT_X, LOG_Y = 96, 286
ADMIN_X = 812
COLORS = {"A": CYAN, "B": INDIGO, "C": VIOLET}
BODY = {"A": "is 200 ok", "B": "is 200 ok", "C": "is 503"}

# (time, kind, detail): a request, or an admin call that keeps or resets the cycle
EVENTS = [
    (0.4, "req", None),
    (2.2, "req", None),
    (4.0, "req", None),
    (5.8, "keep", "PUT …/stubs/by-id/s"),
    (7.8, "req", None),
    (9.6, "req", None),
    (11.4, "req", None),
    (13.2, "reset", "PUT …/stubs/0"),
    (15.0, "req", None),
]


def build() -> Scene:
    sc = Scene(
        1000, 580, T,
        "response cycle · a rotating queue per stub",
        'stub: responses [A (repeat 2), B, C]',
        "each request takes the head; after `repeat` serves it rotates to the tail; replacing the stub at its index restarts it",
        "Response cycling in a Rift stub with responses A (repeat 2), B and C: each request is answered by the "
        "response at the head of the queue, which moves to the tail once it has been served repeat times, so "
        "requests get A, A, B, C, A. Replacing the stub by id keeps the cycle; replacing it at its index resets "
        "it to A. Captions list the other resets (PUT /imposters, DELETE savedProxyResponses) and what keeps "
        "the cycle (replace by id, a reload that leaves the stub unchanged).",
        still=T - 0.4,
    )
    sc.add(sc.text(28, 86, "CLIENT", "tiny"), sc.text(SLOT_X[0] - CARD_W / 2, 86, "STUB RESPONSES · head → tail",
                                                      "tiny"), sc.text(ADMIN_X, 86, "ADMIN API", "tiny", anchor="middle"))
    sc.add(sc.panel(SLOT_X[0] - CARD_W / 2 - 18, 100, SLOT_X[2] - SLOT_X[0] + CARD_W + 36, 172))
    for i, x in enumerate(SLOT_X):
        sc.add(f'<rect x="{x - CARD_W / 2}" y="{CARD_Y - CARD_H / 2}" width="{CARD_W}" height="{CARD_H}" rx="8" '
               f'fill="none" stroke="{FAINT}" stroke-dasharray="3 5"/>')
    sc.add(sc.text(SLOT_X[0], CARD_Y + 52, "head · serves next", "tiny", anchor="middle"),
           sc.text(SLOT_X[2], CARD_Y + 52, "tail", "tiny", anchor="middle"))
    sc.add(sc.text(28, LOG_Y - 18, "served", "tiny"))

    # Simulate the queue and collect each card's keyframes
    order, count = ["A", "B", "C"], 0
    track = {k: [(0.0, SLOT_X[i] - SLOT_X[i], 0)] for i, k in enumerate(order)}
    home = {k: SLOT_X[i] for i, k in enumerate(order)}
    pip_windows: list[list[tuple[float, float]]] = [[], []]
    pip_on: list[float | None] = [None, None]
    served = []

    def place(k, t, slot, lift=0.0):
        track[k].append((t, SLOT_X[slot] - home[k], lift))

    for t, kind, detail in EVENTS:
        if kind == "req":
            head = order[0]
            serve = t + 0.6
            served.append((serve, head))
            sc.add(sc.g(sc.path([(t, 0, 0, 0), (t + 0.1, 0, 0), (serve, SLOT_X[0] - CARD_W / 2 - 40 - CLIENT_X, 0),
                                 (serve + 0.1, SLOT_X[0] - CARD_W / 2 - 40 - CLIENT_X, 0, 0)]),
                        sc.chip(CLIENT_X, CARD_Y, f"req {len(served)}", TEXT, glow=False)))
            sc.add(sc.pulse(SLOT_X[0], CARD_Y, COLORS[head], serve, 30))
            rotate = True
            if head == "A":
                pip_on[count] = serve
                count += 1
                rotate = count == 2
            if rotate:
                if head == "A":
                    for i in range(2):
                        pip_windows[i].append((pip_on[i], serve + 0.9))
                    count = 0
                r = serve + 0.25
                place(head, r, 0)
                place(head, r + 0.3, 0, LIFT)
                place(head, r + 0.7, 2, LIFT)
                place(head, r + 1.0, 2)
                for i, k in enumerate(order[1:], start=1):
                    place(k, r + 0.3, i)
                    place(k, r + 0.7, i - 1)
                order = order[1:] + order[:1]
        else:
            color = GREEN if kind == "keep" else AMBER
            result = "cycle kept" if kind == "keep" else "cycle reset → A"
            hit = t + 0.8
            sc.add(sc.g(sc.path([(t, 60, 0, 0), (t + 0.2, 60, 0), (hit, 0, 0), (T if kind == "reset" else hit + 1.6, 0, 0),
                                 (T if kind == "reset" else hit + 1.9, 0, 0, 0 if kind == "keep" else 1)]),
                        sc.chip(ADMIN_X, CARD_Y - 20, detail, color)))
            sc.add(sc.g(sc.shown([(hit, T if kind == "reset" else hit + 1.9)]),
                        sc.text(ADMIN_X, CARD_Y + 14, result, "mark", fill=color, anchor="middle")))
            if kind == "reset":
                if count:
                    for i in range(count):
                        pip_windows[i].append((pip_on[i], hit))
                count = 0
                r = hit + 0.1
                for i, k in enumerate(order):
                    place(k, r, i)
                    place(k, r + 0.3, i, LIFT)
                for i, k in enumerate(["A", "B", "C"]):
                    place(k, r + 0.8, i, LIFT)
                    place(k, r + 1.1, i)
                order = ["A", "B", "C"]
    for i in range(count):
        pip_windows[i].append((pip_on[i], T))

    # The cards
    for k in ["A", "B", "C"]:
        x, c = home[k], COLORS[k]
        parts = [f'<rect x="{x - CARD_W / 2}" y="{CARD_Y - CARD_H / 2}" width="{CARD_W}" height="{CARD_H}" rx="8" '
                 f'fill="#15172b" stroke="{c}" stroke-width="1.6" filter="url(#glow)"/>',
                 f'<rect x="{x - CARD_W / 2}" y="{CARD_Y - CARD_H / 2}" width="{CARD_W}" height="{CARD_H}" rx="8" '
                 f'fill="{c}" fill-opacity=".12"/>',
                 sc.text(x - 36, CARD_Y - 6, k, "title", fill=c),
                 sc.text(x - 36, CARD_Y + 14, BODY[k], "cap")]
        if k == "A":
            parts.append(sc.text(x - 14, CARD_Y - 6, "repeat 2", "cap", fill=c))
            for i in range(2):
                cx = x + 30 + i * 14
                parts.append(f'<circle cx="{cx}" cy="{CARD_Y + 22}" r="4.5" fill="none" stroke="{c}"/>')
                parts.append(sc.g(sc.shown(pip_windows[i], 0.1),
                                  f'<circle cx="{cx}" cy="{CARD_Y + 22}" r="4.5" fill="{c}"/>'))
        sc.add(sc.g(sc.path([(t, dx, dy) for t, dx, dy in track[k]]), *parts))

    sc.add(sc.arrow(ADMIN_X - 100, CARD_Y - 20, SLOT_X[2] + CARD_W / 2 + 22, CARD_Y - 20, FAINT))

    # The client's log of what it was served
    for n, (t, k) in enumerate(served):
        y = LOG_Y + n * 22
        sc.add(sc.g(sc.shown([(t + 0.8, T - 0.1)], 0.2), sc.text(28, y + 4, f"req {n + 1} →", "lbl", fill=DIM)))
        sc.add(sc.g(sc.path([(t + 0.1, SLOT_X[0] - CLIENT_X - 20, CARD_Y - y, 0),
                             (t + 0.2, SLOT_X[0] - CLIENT_X - 20, CARD_Y - y), (t + 1.0, 0, 0), (T - 0.3, 0, 0),
                             (T, 0, 0, 0)]),
                    sc.text(CLIENT_X + 20, y + 4, k, "big", fill=COLORS[k])))

    # What resets and what keeps the cycle
    y0 = 296
    sc.add(sc.panel(SLOT_X[0] - CARD_W / 2 - 18, y0, 972 - (SLOT_X[0] - CARD_W / 2 - 18), 218, "#0b0c18"))
    x0 = SLOT_X[0] - CARD_W / 2
    sc.add(sc.text(x0, y0 + 26, "RESETS THE CYCLE (back to the first response)", "tiny", fill=AMBER))
    resets = ["PUT /imposters/:port/stubs/:index — replace at index, even unchanged  #1515",
              "PUT /imposters — every imposter in the body, unless ?reconcile=true  #1518",
              "DELETE /imposters/:port/savedProxyResponses — every stub restarts  #1519",
              "deleting and recreating the imposter; replacing the whole stub list"]
    for i, s in enumerate(resets):
        sc.add(sc.text(x0, y0 + 48 + i * 19, "· " + s, "cap"))
    sc.add(sc.text(x0, y0 + 136, "KEEPS THE CYCLE", "tiny", fill=GREEN))
    keeps = ["PUT /imposters/:port/stubs/by-id/:id — replace by id (a Rift extension)",
             "a reload that leaves the stub unchanged, e.g. PUT /imposters?reconcile=true",
             "inserting, deleting or replacing another stub; moving this one"]
    for i, s in enumerate(keeps):
        sc.add(sc.text(x0, y0 + 158 + i * 19, "· " + s, "cap"))
    return sc
