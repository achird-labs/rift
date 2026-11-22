#!/usr/bin/env python3
"""Check that recorded runs of the engine are behaviours of the scenario model.

Usage: python3 validate.py <tla2tools.jar> <trace dir or files...>
       python3 validate.py <tla2tools.jar> --selftest traces
       python3 validate.py <tla2tools.jar> --mutants traces/accept

`--selftest` checks the checker: every trace under `traces/accept` must be accepted and every one
under `traces/reject` rejected (runs the model forbids, written by hand from recorded ones).
`--mutants` checks that validation has teeth: each mutant below, a bug the engine was fixed for put
back into Scenario.tla, must make at least one accepted trace rejected.

Each trace is ndjson written by crates/rift-mock-core/tests/scenario_interleavings.rs under
FORMAL_TRACE_DIR: a `Start` line with how many requests of each role the run made (`gated`,
`pollers`, `setters`, `resetters`, `clearers`), then one line per probe point.  The store starts
empty and a setter writes `B`, as on the model's stub table, which every test uses.  TLC searches
TraceValidation.tla for a behaviour of Scenario that takes those steps in that order.  Exits
non-zero when a trace is rejected or none was accepted.
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

# Bugs the scenario code was fixed for, as mutants.py puts them back, that a recorded run must catch.
# #1779 (a late publish overwrites a higher stamp) is not here: the only thing that reads the map is
# a request's copy, which fires no point, so TLC may place a later request's copy before the stalled
# publish and every trace still passes.  mutants.py catches it with StampMonotone.
MUTANTS = {
    'each gate reads the state again (#1781)': [
        ('st == IF i = 1 THEN Logical(store) ELSE req[r].seen', 'st == Logical(store)')],
    'a gated transition is a plain set (#311)': [
        ('/\\ IF store = req[r].exp\n', '/\\ IF TRUE\n')],
    'a set decides before its write (#1780)': [
        ('IF req[r].exp = store\n', 'IF TRUE\n')],
    'a clear does not re-check the stamp map (#1780)': [
        ('IF @.held \\/ map # @.before', 'IF @.held')],
}

ROLES = (('gated', 'Gated', 'g'), ('pollers', 'Pollers', 'p'), ('setters', 'Setters', 's'),
         ('resetters', 'Resetters', 'd'), ('clearers', 'Clearers', 'c'))


def labels(path):
    """(start, [step]) of a trace file, each step a dict with `action`, `role`, `n`, `changed`.

    A stamp is already the model's counter: the test writes the k-th stamp issued as k, a stub's own
    load stamp as 0, and any other as -1, which no step serves.
    """
    with open(path) as f:
        lines = [json.loads(line) for line in f if line.strip()]
    if not lines or lines[0].get('point') != 'Start':
        raise ValueError(f'{path}: no Start line')
    steps = []
    for p in lines[1:]:
        if p['point'] in NO_STEP:
            continue
        steps.append({'action': p['point'], 'role': p.get('role', ''), 'n': p.get('stamp', p.get('pos', 0)),
                      'changed': p.get('changed', False)})
    return lines[0], steps


def check(jar, path, spec=None):
    start, steps = labels(path)
    recs = ', '.join(
        f'[action |-> "{s["action"]}", role |-> "{s["role"]}", n |-> {s["n"]}, '
        f'changed |-> {"TRUE" if s["changed"] else "FALSE"}]' for s in steps)
    trace = f'''---- MODULE Trace ----
EXTENDS Sequences
Trace == <<{recs}>>
====
'''
    sets = '\n'.join(
        f'    {constant} = {{{", ".join(f"{prefix}{k + 1}" for k in range(start[key]))}}}'
        for key, constant, prefix in ROLES)
    cfg = f"""CONSTANTS
{sets}
    Start = NoVal
    SetTo = "B"
    AtomicCas = TRUE
    NoVal = NoVal
INIT TInit
NEXT TraceNext
INVARIANT TraceNotDone
"""
    extra = {'Scenario': spec or read(os.path.join(here, 'Scenario.tla')), 'Trace': trace}
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
        spec = read(os.path.join(here, 'Scenario.tla'))
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
            print(f'  {[s["action"] for s in steps]}\n  {why}')
    sys.exit(1 if rejected or not paths else 0)


if __name__ == '__main__':
    main()
