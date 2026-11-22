#!/usr/bin/env python3
"""Check that recorded runs of the engine are behaviours of the script state cell model.

Usage: python3 validate.py <tla2tools.jar> <trace dir or files...>
       python3 validate.py <tla2tools.jar> --selftest traces
       python3 validate.py <tla2tools.jar> --mutants traces/accept

`--selftest` checks the checker: every trace under `traces/accept` must be accepted and every one
under `traces/reject` rejected (runs the model forbids, written by hand from recorded ones).
`--mutants` checks that validation has teeth: each mutant below, a bug the engine was fixed for put
back into StateCell.tla, must make at least one accepted trace rejected.

Each trace is ndjson written by crates/rift-mock-core/tests/scripting_interleavings.rs under
FORMAL_TRACE_DIR: a `Start` line with the run's configuration (`runs`, the script runs that started;
`restarts`; `accessChanges`), then one line per state cell point with its cell's `key`, numbered in
order of first appearance.  TLC searches TraceValidation.tla for a behaviour of StateCell from that
configuration that takes those steps in that order.  Exits non-zero when a trace is rejected or none
was accepted.
"""
import glob
import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
# Points with no step of their own.
NO_STEP = {'Other'}

SAVE_GUARD = '''EndSave(r) ==
    /\\ run[r].pc = "save"
'''
SAVE_NEXT = '''run' = [run EXCEPT ![r].pc = "finish",'''
ANSWER_GUARD = '''FinishAnswered(r) ==
    /\\ run[r].pc = "finish"
'''
ANSWER_NEXT = '''run' = [run EXCEPT ![r].pc = "release", ![r].answered'''

# Bugs the script state code was fixed for, as mutants.py puts them back, that a recorded run must
# catch.  Most of mutants.py's are lost writes (#1785, #1726, #477), which the points cannot see: they
# carry a cell, not what it holds.  Others add behaviours (#1786, a route ignored, no deferral), and a
# trace of the fixed engine is still one of them.  README.md, "Trace validation".
MUTANTS = {
    'the answer goes out before the save (#1678)': [
        (SAVE_GUARD, SAVE_GUARD.replace('"save"', '"finish"')),
        (SAVE_NEXT, SAVE_NEXT.replace('"finish"', '"release"')),
        (ANSWER_GUARD, ANSWER_GUARD.replace('"finish"', '"save"')),
        (ANSWER_NEXT, ANSWER_NEXT.replace('"release"', '"finish"'))],
}


def labels(path):
    """(start, [(action, key)]) of a trace file."""
    with open(path) as f:
        lines = [json.loads(line) for line in f if line.strip()]
    if not lines or lines[0].get('point') != 'Start':
        raise ValueError(f'{path}: no Start line')
    steps = [(line['point'], line['key']) for line in lines[1:] if line['point'] not in NO_STEP]
    return lines[0], steps


def check(jar, path, spec=None):
    start, steps = labels(path)
    recs = ', '.join(f'[action |-> "{a}", key |-> {k}]' for a, k in steps)
    keys = max([k for _, k in steps], default=0)
    trace = f'''---- MODULE Trace ----
EXTENDS Sequences
Trace == <<{recs}>>
TKeys == {keys}
====
'''
    runs = ', '.join(f'r{k + 1}' for k in range(start['runs']))
    accesses = '{a1, a2}' if start['accessChanges'] else '{a1}'
    cfg = f"""CONSTANTS
    Runs = {{{runs}}}
    r1 = r1
    r2 = r2
    Workers = {{w1, w2}}
    Accesses = {accesses}
    NoWorker = NoWorker
    NoRun = NoRun
    MaxJobs = 1
    MaxRestarts = {start['restarts']}
    MaxAccessChanges = {start['accessChanges']}
    Sequels = FALSE
    GetterWrites = TRUE
    SaveFails = FALSE
    Panics = FALSE
SYMMETRY WorkerSymmetry
INIT TInit
NEXT TraceNext
INVARIANT TraceNotDone
"""
    extra = {'StateCell': spec or read(os.path.join(here, 'StateCell.tla')), 'Trace': trace}
    verdict, _, out = tlc(jar, 'TraceValidation', read(os.path.join(here, 'TraceValidation.tla')), cfg,
                          extra_modules=extra)
    if verdict == 'TraceNotDone':
        return True, steps, ''
    if verdict in ('ok', 'deadlock'):
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
    if sys.argv[2] == '--mutants':
        paths = collect(sys.argv[3:])
        spec = read(os.path.join(here, 'StateCell.tla'))
        missed = 0
        for name, edits in MUTANTS.items():
            mutated = edit(spec, edits, name)
            caught = [os.path.basename(p) for p in paths if not check(jar, p, mutated)[0]]
            print(f'{name}: {"rejects " + ", ".join(caught) if caught else "MISSED: every trace still accepted"}')
            missed += not caught
        sys.exit(1 if missed else 0)
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
