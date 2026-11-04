#!/usr/bin/env python3
"""Check that recorded runs of the engine are behaviours of the proxyOnce model.

Usage: python3 validate.py <tla2tools.jar> <trace dir or files...>
       python3 validate.py <tla2tools.jar> --selftest traces

`--selftest` checks the checker: every trace under `traces/accept` must be accepted and every one
under `traces/reject` rejected (runs the model forbids, written by hand from recorded ones).

Each trace is ndjson written by crates/rift-mock-core/tests/proxyonce_interleavings.rs under
RIFT_TRACE_DIR: a `Start` line saying whether the run starts recorded, then one line per probe
point.  TLC searches TraceValidation.tla for a behaviour of ProxyOnce that takes those steps in that
order.  Exits non-zero when a trace is rejected or when none was accepted.
"""
import glob
import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
# Points with no step of their own: the hold points, and those a real commit point follows.
NO_STEP = {'ProxyLookingUp', 'ProxyOrphanFound', 'Other'}


def labels(path):
    """(preloaded, [(action, arg)]) of a trace file."""
    with open(path) as f:
        lines = [json.loads(line) for line in f if line.strip()]
    if not lines or lines[0].get('point') != 'Start':
        raise ValueError(f'{path}: no Start line')
    steps = []
    for line in lines[1:]:
        name = line['point']
        if name in NO_STEP:
            continue
        arg = line.get('outcome', '')
        if name == 'ProxyInserted':
            arg = 'claimed' if line['claimed'] else 'unclaimed'
        steps.append((name, arg))
    return lines[0]['preloaded'], steps


def check(jar, path):
    preloaded, steps = labels(path)
    recs = ', '.join(f'[action |-> "{a}", arg |-> "{b}"]' for a, b in steps)
    trace = f'---- MODULE Trace ----\nEXTENDS Sequences\nTrace == <<{recs}>>\n====\n'
    cfg = f"""CONSTANTS
    Requests = {{r1, r2, r3}}
    Editors = {{e1, e2}}
    EditKinds = {{"move", "delete", "replace", "deleteProxy", "replaceAll"}}
    Clearers = {{c1}}
    Restarters = {{}}
    GeneratesStub = TRUE
    SettleCanFail = FALSE
    Preloaded = {'TRUE' if preloaded else 'FALSE'}
INIT TInit
NEXT TraceNext
INVARIANT TraceNotDone
"""
    extra = {'ProxyOnce': read(os.path.join(here, 'ProxyOnce.tla')), 'Trace': trace}
    verdict, _, out = tlc(jar, 'TraceValidation', read(os.path.join(here, 'TraceValidation.tla')), cfg,
                          extra_modules=extra)
    if verdict == 'TraceNotDone':
        return True, steps, ''
    if verdict == 'ok':
        return False, steps, 'no behaviour of the model takes these steps'
    return False, steps, 'TLC failed: ' + out[-1200:]


def collect(args):
    paths = []
    for arg in args:
        paths += sorted(glob.glob(os.path.join(arg, '*.ndjson'))) if os.path.isdir(arg) else [arg]
    return paths


def main():
    jar = os.path.abspath(sys.argv[1])
    if sys.argv[2] == '--selftest':
        root = sys.argv[3]
        wrong = 0
        for expected, sub in ((True, 'accept'), (False, 'reject')):
            for path in collect([os.path.join(root, sub)]):
                accepted, _, why = check(jar, path)
                print(f'{"accepted" if accepted else "rejected"}: {sub}/{os.path.basename(path)}')
                if accepted != expected:
                    wrong += 1
                    print(f'  WRONG: {why}')
        sys.exit(1 if wrong else 0)
    paths = collect(sys.argv[2:])
    rejected = 0
    for path in paths:
        accepted, steps, why = check(jar, path)
        print(f'{"accepted" if accepted else "REJECTED"}: {os.path.basename(path)}')
        if not accepted:
            rejected += 1
            print(f'  {[a for a, _ in steps]}\n  {why}')
    sys.exit(1 if rejected or not paths else 0)


if __name__ == '__main__':
    main()
