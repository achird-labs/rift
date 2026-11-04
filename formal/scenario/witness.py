#!/usr/bin/env python3
"""Check that the scenario model's documented limits, and the defects it found that are still open,
stay reachable, and that the fix each finding proposes closes it in the model: each case runs TLC
with one change and requires exactly the expected verdict.

Usage: python3 witness.py <path to tla2tools.jar>.  A limit that became unreachable means the code
or the model changed, and README.md must change with it; a finding that became unreachable means
its fix landed, and its invariant moves back to ScenarioPR.cfg.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'Scenario.tla'))
pr_cfg = read(os.path.join(here, 'ScenarioPR.cfg'))
known_cfg = read(os.path.join(here, 'Known.cfg'))
witness_cfg = read(os.path.join(here, 'Witness.cfg'))
jar = jar_from_argv()


def only(invariant, cfg=known_cfg):
    """`cfg` with `invariant` as its only invariant."""
    return f'{cfg.split("INVARIANTS")[0]}INVARIANTS\n    {invariant}\n'


def bounds(invariant, gated, pollers, setters, resetters, start='"B"'):
    """`invariant` alone, on the named requests, so one finding is isolated from the others."""
    cfg = f'''CONSTANTS
    Gated = {{{gated}}}
    Pollers = {{{pollers}}}
    Setters = {{{setters}}}
    Resetters = {{{resetters}}}
    Start = {start}
    SetTo = "B"
    AtomicCas = TRUE
    NoVal = NoVal

SPECIFICATION Spec

SYMMETRY Symmetry

INVARIANTS
    {invariant}
'''
    return cfg


# The fix each finding proposes, as an edit of the model.
MONOTONE_PUBLISH = [("/\\ map' = req[r].at\n", "/\\ map' = Max(map, req[r].at)\n")]
# A scan reads each scenario once, so every stub is judged against one state.
ONE_READ_SCAN = [('IN IF st = Order[i]\n', 'IN IF TRUE\n')]
# The store reports the value it replaced, so "did this move it" is decided with the write.
WRITE_REPORTS_CHANGE = [
    ('''    /\\ req' = [req EXCEPT ![r].changed = (Logical(store) # SetTo), ![r].to = SetTo,
                          ![r].pc = "setWrite"]
    /\\ UNCHANGED <<store, map, clock, ver, writes, served, maxPub, casBroken, dropped, noMatch>>''',
     '''    /\\ req' = [req EXCEPT ![r].changed = (Logical(store) # SetTo), ![r].to = SetTo,
                          ![r].wrote = ver + 1,
                          ![r].pc = IF Logical(store) # SetTo THEN "issue" ELSE "done"]
    /\\ store' = SetTo
    /\\ ver' = ver + 1
    /\\ UNCHANGED <<map, clock, writes, served, maxPub, casBroken, dropped, noMatch>>'''),
    ('''    /\\ req' = [req EXCEPT ![r].changed = (store # NoVal), ![r].to = "S", ![r].pc = "delWrite"]
    /\\ UNCHANGED <<store, map, clock, ver, writes, served, maxPub, casBroken, dropped, noMatch>>''',
     '''    /\\ req' = [req EXCEPT ![r].changed = (store # NoVal), ![r].to = "S",
                          ![r].wrote = ver + 1,
                          ![r].pc = IF store # NoVal THEN "issue" ELSE "done"]
    /\\ store' = NoVal
    /\\ ver' = ver + 1
    /\\ UNCHANGED <<map, clock, writes, served, maxPub, casBroken, dropped, noMatch>>'''),
]

cases = [
    # The race the protocol depends on, and the documented limit of the trait's default.
    ('a compare-and-set can lose (matching.rs:705)', [], witness_cfg, 'NeverDroppedTransition'),
    ('the default compare_and_set overwrites a state it did not expect (flow_state.rs:100)', [],
     edit(only('TransitionsAreCas', witness_cfg), [('AtomicCas = TRUE', 'AtomicCas = FALSE')], 'Witness.cfg'),
     'TransitionsAreCas'),
    ('the same, with an atomic compare_and_set (control)', [], only('TransitionsAreCas', witness_cfg), 'ok'),
    # Filed findings, reachable until fixed (README.md, "Findings").
    ('a late rcu puts a lower stamp over a higher one (#1779)', [], only('StampMonotone'), 'StampMonotone'),
    ('a client can hold an answer it is never told changed (#1779, #1780)', [],
     only('NoStaleAnswerAtRest'), 'NoStaleAnswerAtRest'),
    ('a scan of covered states matches nothing (#1781)', [], only('NoMatchOnlyWhenUncovered'),
     'NoMatchOnlyWhenUncovered'),
    # Each finding alone, then under its proposed fix.
    ('the regression alone: three transitions, nothing else', [],
     bounds('NoStaleAnswerAtRest', 'g1, g2, g3', '', '', ''), 'NoStaleAnswerAtRest'),
    ('the same, publishing the higher stamp', MONOTONE_PUBLISH,
     bounds('NoStaleAnswerAtRest', 'g1, g2, g3', '', '', ''), 'ok'),
    ('the same, StampMonotone', MONOTONE_PUBLISH, bounds('StampMonotone', 'g1, g2, g3', '', '', ''), 'ok'),
    ('the unstamped set alone: a transition, a poller, a setter', [],
     bounds('NoStaleAnswerAtRest', 'g1', 'p1', 's1', ''), 'NoStaleAnswerAtRest'),
    ('the same, the write deciding the change', WRITE_REPORTS_CHANGE,
     bounds('NoStaleAnswerAtRest', 'g1', 'p1', 's1', ''), 'ok'),
    ('the unstamped delete alone: a transition, a poller, an admin delete', [],
     bounds('NoStaleAnswerAtRest', 'g1', 'p1', '', 'd1', 'NoVal'), 'NoStaleAnswerAtRest'),
    ('the same, the write deciding the change', WRITE_REPORTS_CHANGE,
     bounds('NoStaleAnswerAtRest', 'g1', 'p1', '', 'd1', 'NoVal'), 'ok'),
    ('the torn scan alone: a transition and a poller', [],
     bounds('NoMatchOnlyWhenUncovered', 'g1', 'p1', '', ''), 'NoMatchOnlyWhenUncovered'),
    ('the same, one read per scan', ONE_READ_SCAN,
     bounds('NoMatchOnlyWhenUncovered', 'g1', 'p1', '', ''), 'ok'),
    # The PR gate is green with those three in Known.cfg.
    ('ScenarioPR.cfg (control)', [], pr_cfg, 'ok'),
]
failed = False
for name, spec_edits, cfg, expected in cases:
    verdict, trace, out = tlc(jar, 'Scenario', edit(spec, spec_edits, name), cfg)
    if verdict == expected:
        print(f'{name}: {verdict}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
