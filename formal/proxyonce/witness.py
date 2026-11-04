#!/usr/bin/env python3
"""Check that the races the proxyOnce model depends on, its documented limits, and the defects it
found that are still open, stay reachable: each case runs TLC with one change and requires exactly
the expected verdict.

Usage: python3 witness.py <path to tla2tools.jar>.  A witness that became unreachable means the code
or the model changed, and README.md must change with it; a finding that became unreachable means its
fix landed, and its invariant moves back to ProxyOncePR.cfg.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'ProxyOnce.tla'))
pr_cfg = read(os.path.join(here, 'ProxyOncePR.cfg'))
jar = jar_from_argv()


def only(cfg, invariant):
    """`cfg` with `invariant` as its only invariant."""
    head = cfg.split('INVARIANTS')[0]
    return f'{head}INVARIANTS\n    {invariant}\n'



cases = [
    # The races: a model where these stayed false would check nothing.
    ('two requests reach the upstream', only(pr_cfg, 'NeverTwoForwards'), 'NeverTwoForwards'),
    ('a forget runs while a claim is in flight (proxy_store.rs:273)', only(pr_cfg, 'NeverForgetWhileClaimed'),
     'NeverForgetWhileClaimed'),
    # Documented limits: a clear or a restart can leave what these invariants rule out.
    ('a clear or a restart can leave a second carrier ahead of the proxy',
     only(pr_cfg, 'AtMostOneCarrier'), 'AtMostOneCarrier'),
    ('a clear or a restart can leave a carrier with no recording',
     only(pr_cfg, 'CarrierHasRecording'), 'CarrierHasRecording'),
    # A proxy that generates no stub: no carrier, no shadow, no forget from an edit.
    ('no stub generated: every invariant holds (control)',
     edit(pr_cfg, [('GeneratesStub = TRUE', 'GeneratesStub = FALSE'), ('Preloaded = TRUE', 'Preloaded = FALSE')],
          'ProxyOncePR.cfg') + '    AtMostOneCarrier\n',
     'ok'),
    # The PR gate is green.
    ('ProxyOncePR.cfg (control)', pr_cfg, 'ok'),
]
failed = False
for name, cfg, expected in cases:
    verdict, trace, out = tlc(jar, 'ProxyOnce', spec, cfg)
    if verdict == expected:
        print(f'{name}: {verdict}')
        if trace:
            print(f'  {" -> ".join(trace)}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
