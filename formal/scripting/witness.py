#!/usr/bin/env python3
"""Check that what the state cell's properties rest on stays reachable, and that the defects the model
found stay reachable until fixed: each case runs TLC with one cfg and requires exactly the expected verdict.

Usage: python3 witness.py <path to tla2tools.jar>.  A witness that holds means the model lost a race the
README describes; a finding that holds means its fix landed, and README.md changes with it.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'StateCell.tla'))
pr_cfg = read(os.path.join(here, 'StateCellPR.cfg'))
witness_cfg = read(os.path.join(here, 'Witness.cfg'))
jar = jar_from_argv()


def only(cfg, invariant):
    """`cfg` with `invariant` as its only invariant."""
    head = cfg.split('INVARIANTS')[0]
    return f'{head}INVARIANTS\n    {invariant}\n'


def flag(cfg, name, value):
    return edit(cfg, [(f'{name} = FALSE', f'{name} = {value}')], name)


cases = [
    # The races the properties depend on.
    ('a run in flight across a restart saves to the cell the imposter no longer has',
     only(pr_cfg, 'NeverSaveToOldCell'), 'NeverSaveToOldCell'),
    ('runs of one imposter on two workers, once a restart made them two keys',
     only(pr_cfg, 'NeverTwoWorkersOneImposter'), 'NeverTwoWorkersOneImposter'),
    ('a start waits for its imposter to leave another realm', only(pr_cfg, 'NeverDeferred'), 'NeverDeferred'),
    ('a run inherits the jobs a finished kin left', only(pr_cfg, 'NeverInherits'), 'NeverInherits'),
    ('a run in flight is dropped once its caller left', only(pr_cfg, 'NeverAbandoned'), 'NeverAbandoned'),
    # Documented limits.
    ('a state js_to_json refuses is lost at the last save (#1635)',
     flag(only(pr_cfg, 'NoLostUpdate'), 'SaveFails', 'TRUE'), 'NoLostUpdate'),
    ('a panicking job loses its worker\'s unsaved writes (js_engine run_mb_worker)',
     flag(only(pr_cfg, 'NoLostUpdate'), 'Panics', 'TRUE'), 'NoLostUpdate'),
    # Filed findings, reachable until fixed (README.md, "Findings").
    # The gates are green.
    ('StateCellPR.cfg (control; includes NeverTwoWorkersOneKey)', pr_cfg, 'ok'),
    ('Witness.cfg: every failure on, and affinity, one live state and the cells still hold (control)',
     witness_cfg, 'ok'),
]
failed = False
for name, cfg, expected in cases:
    verdict, trace, out = tlc(jar, 'StateCell', spec, cfg, deadlock=False)
    if verdict == expected:
        print(f'{name}: {verdict}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
