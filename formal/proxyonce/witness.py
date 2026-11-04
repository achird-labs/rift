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
known_cfg = read(os.path.join(here, 'Known.cfg'))
jar = jar_from_argv()


def only(cfg, invariant):
    """`cfg` with `invariant` as its only invariant."""
    head = cfg.split('INVARIANTS')[0]
    return f'{head}INVARIANTS\n    {invariant}\n'


def no_clear(cfg):
    return edit(cfg, [('Clearers = {c1}', 'Clearers = {}')], 'Known.cfg')


cases = [
    # The races: a model where these stayed false would check nothing.
    ('two requests reach the upstream', only(pr_cfg, 'NeverTwoForwards'), 'NeverTwoForwards'),
    ('a forget runs while a claim is in flight (proxy_store.rs:273)', only(pr_cfg, 'NeverForgetWhileClaimed'),
     'NeverForgetWhileClaimed'),
    # Filed findings, reachable until fixed (README.md, "Findings").
    ('a forget removes a recording newer than the one it decided about (#1776)',
     only(no_clear(known_cfg), 'ForgetRemovesWhatItSaw'), 'ForgetRemovesWhatItSaw'),
    ('that leaves two recordings ahead of the proxy, with no clear in the run (#1776)',
     only(no_clear(known_cfg), 'AtMostOneCarrier'), 'AtMostOneCarrier'),
    ('that leaves a live stub with no recording, with no clear in the run (#1776)',
     only(no_clear(known_cfg), 'CarrierHasRecording'), 'CarrierHasRecording'),
    ('a stale token still inserts a stub carrying the claim (#1777)',
     only(known_cfg, 'CarrierMeansRecorded'), 'CarrierMeansRecorded'),
    # A proxy that generates no stub: no carrier, no shadow, no forget from an edit.
    ('no stub generated: every invariant holds (control)',
     edit(known_cfg, [('GeneratesStub = TRUE', 'GeneratesStub = FALSE'), ('Preloaded = TRUE', 'Preloaded = FALSE'),
                      ('    AtMostOneCarrier\n    ForgetRemovesWhatItSaw\n    CarrierHasRecording\n'
                       '    CarrierMeansRecorded\n',
                       '    AtMostOneCarrier\n    NoWedgedClaim\n    ClaimExcludesRecording\n    RecordedOnce\n'
                       '    ForgetRemovesWhatItSaw\n    OnlyClaimHolderInserts\n')], 'Known.cfg'),
     'ok'),
    # The PR gate is green with the findings in Known.cfg.
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
