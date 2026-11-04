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
witness_cfg = read(os.path.join(here, 'Witness.cfg'))
jar = jar_from_argv()


def only(invariant, cfg):
    """`cfg` with `invariant` as its only invariant."""
    return f'{cfg.split("INVARIANTS")[0]}INVARIANTS\n    {invariant}\n'


def bounds(invariant, gated, pollers, setters, resetters, start='"B"', clearers=''):
    """`invariant` alone, on the named requests, so one finding is isolated from the others."""
    cfg = f'''CONSTANTS
    Gated = {{{gated}}}
    Pollers = {{{pollers}}}
    Setters = {{{setters}}}
    Resetters = {{{resetters}}}
    Clearers = {{{clearers}}}
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
cases = [
    # The race the protocol depends on, and the documented limit of the trait's default.
    ('a compare-and-set can lose (matching.rs:705)', [], witness_cfg, 'NeverDroppedTransition'),
    ('the default compare_and_set overwrites a state it did not expect (flow_state.rs:100)', [],
     edit(only('TransitionsAreCas', witness_cfg), [('AtomicCas = TRUE', 'AtomicCas = FALSE')], 'Witness.cfg'),
     'TransitionsAreCas'),
    ('the same, with an atomic compare_and_set (control)', [], only('TransitionsAreCas', witness_cfg), 'ok'),
    # A clear racing a transition, which the larger cfgs leave out.
    ('a clear racing a transition stamps what it moved (control)', [],
     bounds('NoStaleAnswerAtRest', 'g1', 'p1', '', '', 'NoVal', 'c1'), 'ok'),
    # The PR gate is green.
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
