#!/usr/bin/env python3
"""Check that the lifecycle model's documented limits, and the defects it found that are still open,
stay reachable: each case runs TLC with one change and requires exactly the expected verdict.

Usage: python3 witness.py <path to tla2tools.jar>.  A limit that became unreachable means the code
or the model changed, and README.md must change with it; a finding that became unreachable means
its fix landed, and its invariant moves back to LifecyclePR.cfg.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'Lifecycle.tla'))
pr_cfg = read(os.path.join(here, 'LifecyclePR.cfg'))
known_cfg = read(os.path.join(here, 'Known.cfg'))
mutants_cfg = read(os.path.join(here, 'Mutants.cfg'))
apply_cfg = read(os.path.join(here, 'LifecycleApply.cfg'))
jar = jar_from_argv()


def only(cfg, invariant):
    """`cfg` with `invariant` as its only invariant."""
    head = cfg.split('INVARIANTS')[0]
    return f'{head}INVARIANTS\n    {invariant}\n'


cases = [
    # Documented limits.
    ('a client too slow to read its response is answered after the delete returned',
     edit(only(mutants_cfg, 'NoServeAfterDeleteReturns'), [('DrainTimesOut = FALSE', 'DrainTimesOut = TRUE')],
          'Mutants.cfg'),
     'NoServeAfterDeleteReturns'),
    ('the same, with the drain waiting (control)', only(mutants_cfg, 'NoServeAfterDeleteReturns'), 'ok'),
    ('two creates pass the bind check and both bind; the claim decides',
     only(known_cfg, 'BindCheckSuffices'), 'BindCheckSuffices'),
    # Filed findings, reachable until fixed (README.md, "Findings").
    ('a create that loses the claim has answered requests', only(known_cfg, 'LoserNeverAnswers'),
     'LoserNeverAnswers'),
    ('a same-port create misses the load floor a concurrent delete records', only(known_cfg, 'FloorMonotone'),
     'FloorMonotone'),
    ('an apply sweep reports a concurrent delete as failed (#1774)',
     only(apply_cfg, 'ApplyReportsTruth'), 'ApplyReportsTruth'),
    # Documented limits.
    # The PR gate is green with the findings in Known.cfg.
    ('LifecyclePR.cfg (control)', pr_cfg, 'ok'),
]
failed = False
for name, cfg, expected in cases:
    verdict, trace, out = tlc(jar, 'Lifecycle', spec, cfg)
    if verdict == expected:
        print(f'{name}: {verdict}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
