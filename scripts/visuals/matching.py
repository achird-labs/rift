"""Predicate matching: stubs are checked in order, the first full match serves, a miss falls through.

Depicts:
- docs/mountebank/imposters.md "Stub Matching Behavior / First-Match-Wins": stubs are evaluated in
  order and the first stub whose predicates all match answers; later stubs are not reached.
- docs/mountebank/predicates.md "Predicate Types" (equals, contains, matches) and "JSONPath
  Predicates" (a `jsonpath` selector combined with an operator).
- crates/rift-mock-core/src/imposter/predicates/mod.rs `stub_matches_explained`: a stub's
  predicates are AND-ed and the scan short-circuits, so predicates after the first failure are never
  evaluated (shown as "-").
- docs/mountebank/imposters.md "Default Response": with no matching stub the imposter's
  `defaultResponse` answers. This scene configures `{"statusCode": 404}`; with none configured Rift
  answers 200 with an empty body (and `x-rift-no-match: true`), as Mountebank does. Both were
  probed against a debug build of master: POST /orders {"tier":"vip"} -> 201 from stub #2, and
  GET /reports -> 404 with `x-rift-default-response: true`.
"""

from hud import CYAN, DIM, FAINT, GREEN, INDIGO, RED, TEXT, VIOLET, Scene

T = 14.0
ROW_Y = [134, 192, 250, 308]
DEFAULT_Y = 382
ROW_X, ROW_W = 240, 610
BADGE_X = 288
VERDICT_X = 676
RESP_X = 770
OUT_X = 925

STUBS = [
    (["equals method=GET", "equals path=/health"], "200 ok"),
    (["equals path=/orders", 'contains body="gold"'], "200 gold"),
    (["equals method=POST", 'jsonpath $.tier · equals "vip"'], "201 created"),
    (["matches path=^/admin"], "200 admin"),
]

# Per request: (start, end, request lines, [(row, [verdict per predicate]), ...], answering row or None, status)
# A verdict is "ok", "no" or "skip" (never evaluated: the scan stopped at an earlier failure).
REQUESTS = [
    (0.4, 7.0, ["POST /orders", '{"id":42,"tier":"vip"}'],
     [(0, ["no", "skip"]), (1, ["ok", "no"]), (2, ["ok", "ok"])], 2, "201 created"),
    (7.2, T, ["GET /reports", "(no body)"],
     [(0, ["ok", "no"]), (1, ["no", "skip"]), (2, ["no", "skip"]), (3, ["no"])], None, "404"),
]


def badge_xs(preds: list[str]) -> list[float]:
    xs, x = [], BADGE_X
    for p in preds:
        xs.append(x)
        x += Scene.width(p + " ✓") + 14 + 10
    return xs


def build() -> Scene:
    sc = Scene(
        1000, 480, T,
        "matching · first full match wins",
        "predicates AND-ed · short-circuit · stubs in order",
        "a stub is skipped at its first failing predicate; no stub matched -> the imposter's defaultResponse",
        "Predicate matching in a Rift imposter: a request is checked against the stubs in order. Each stub's "
        "predicates (equals, contains, jsonpath, matches) are evaluated until one fails, and that stub is "
        "skipped. The first stub whose predicates all match answers, here POST /orders gets 201 from stub 2. "
        "A request no stub matches, GET /reports, falls through to the imposter's defaultResponse, 404 here; "
        "without one Rift answers 200 with an empty body, as Mountebank does.",
        still=T - 0.5,
    )
    sc.add(sc.text(28, 86, "REQUEST", "tiny"), sc.text(ROW_X, 86, "STUBS · checked top to bottom", "tiny"),
           sc.text(VERDICT_X, 86, "VERDICT", "tiny"), sc.text(RESP_X, 86, "RESPONSES", "tiny"),
           sc.text(OUT_X, 86, "SERVED", "tiny", anchor="middle"))

    # The stub list, at rest
    sc.add(sc.panel(ROW_X - 8, 100, ROW_W + 16, 238))
    for i, ((preds, resp), y) in enumerate(zip(STUBS, ROW_Y)):
        sc.add(sc.text(ROW_X + 8, y + 4, f"#{i}", "big", fill=DIM))
        for p, x in zip(preds, badge_xs(preds)):
            sc.add(sc.badge(x, y, p, DIM))
        sc.add(sc.text(RESP_X, y + 4, resp, "lbl", fill=DIM))
    sc.add(sc.box(ROW_X, DEFAULT_Y - 20, ROW_W, 40, FAINT, ".05", dash=True),
           sc.text(ROW_X + 14, DEFAULT_Y + 4, 'defaultResponse  {"statusCode": 404}', "lbl", fill=DIM),
           sc.text(RESP_X, DEFAULT_Y + 4, "404", "lbl", fill=DIM),
           sc.text(ROW_X, DEFAULT_Y + 38, "no defaultResponse configured -> 200 with an empty body, as in Mountebank",
                   "cap"))

    for start, end, lines, checks, answer, status in REQUESTS:
        color = CYAN if answer is not None else VIOLET
        # The request card
        sc.add(sc.g(sc.shown([(start, end)]),
                    sc.box(28, 108, 192, 58, color, ".1"),
                    sc.text(40, 132, lines[0], "big", fill=color),
                    sc.text(40, 152, lines[1], "cap")))
        # The scan cursor: one outline that steps from row to row
        t, pts = start + 0.4, []
        for row, verdicts in checks:
            y = ROW_Y[row] - ROW_Y[0]
            pts += [(t, 0, y), (t + 0.25, 0, y)]
            t += 0.35 + 0.3 * len(verdicts) + 0.3
        last_y = (DEFAULT_Y if answer is None else ROW_Y[answer]) - ROW_Y[0]
        pts += [(t, 0, last_y), (end - 0.3, 0, last_y, 1), (end, 0, last_y, 0)]
        pts = [(start, 0, 0, 0), (start + 0.3, 0, 0, 1)] + [p if len(p) == 4 else (*p, 1) for p in pts]
        sc.add(sc.g(sc.path(pts, "linear"),
                    f'<rect x="{ROW_X - 4}" y="{ROW_Y[0] - 24}" width="{ROW_W + 8}" height="48" rx="7" '
                    f'fill="none" stroke="{color}" stroke-width="1.8" filter="url(#glow)"/>'))
        # Verdicts, predicate by predicate
        t = start + 0.4
        for row, verdicts in checks:
            y = ROW_Y[row]
            preds = STUBS[row][0]
            vt = t + 0.35
            for p, x, v in zip(preds, badge_xs(preds), verdicts):
                if v == "skip":
                    sc.add(sc.g(sc.shown([(vt, end)]), sc.badge(x, y, p + " -", FAINT)))
                else:
                    c, mark = (GREEN, "✓") if v == "ok" else (RED, "✗")
                    sc.add(sc.g(sc.shown([(vt, end)]), sc.badge(x, y, f"{p} {mark}", c)))
                vt += 0.3
            matched = row == answer
            sc.add(sc.g(sc.shown([(vt, end)]),
                        sc.text(VERDICT_X, y + 4, "MATCH" if matched else "skip", "mark",
                                fill=GREEN if matched else RED)))
            if matched:
                sc.add(sc.g(sc.shown([(vt, end)]), sc.text(RESP_X, y + 4, STUBS[row][1], "lbl", fill=GREEN)))
            t = vt + 0.3
        if answer is not None:
            for row in range(answer + 1, len(STUBS)):
                sc.add(sc.g(sc.shown([(t, end)]), sc.text(VERDICT_X, ROW_Y[row] + 4, "not reached", "cap",
                                                          fill=FAINT)))
            y = ROW_Y[answer]
        else:
            y = DEFAULT_Y
            sc.add(sc.g(sc.shown([(t, end)]),
                        sc.text(VERDICT_X, y + 4, "fallback", "mark", fill=INDIGO),
                        sc.text(RESP_X, y + 4, "404", "lbl", fill=TEXT)))
        # The served response travels out
        sc.add(sc.g(sc.path([(t, RESP_X + 40 - OUT_X, 0, 0), (t + 0.2, RESP_X + 40 - OUT_X, 0),
                             (t + 0.9, 0, 0), (end - 0.3, 0, 0), (end, 0, 0, 0)]),
                    sc.chip(OUT_X, y, status, GREEN if answer is not None else INDIGO)))
    return sc
