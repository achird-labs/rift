#!/usr/bin/env python3
"""Check that recorded runs of the engine are behaviours of the connection fault model.

Usage: python3 validate.py <tla2tools.jar> <trace dir or files...>
       python3 validate.py <tla2tools.jar> --selftest traces
       python3 validate.py <tla2tools.jar> --mutants traces/accept

`--selftest` checks the checker: every trace under `traces/accept` must be accepted and every one
under `traces/reject` rejected (runs the model forbids, written by hand from recorded ones).
`--mutants` checks that validation has teeth: each mutant below, a bug the engine was fixed for put
back into FaultIo.tla, must make at least one accepted trace rejected.

Each trace is ndjson written by crates/rift-mock-core/tests/faultio_interleavings.rs under
FORMAL_TRACE_DIR: a `Start` line with the run's configuration (`kinds`, one per request, as the
model names them; `transport`; `version`; `timing`; `pipelines`; `bodyUnits`), then one line per
probe point.  TLC searches TraceValidation.tla for a behaviour of FaultIo from that configuration
that takes those steps in that order.  Exits non-zero when a trace is rejected or none was accepted.
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

# Bugs the fault code was fixed for, as mutants.py puts them back, that a recorded run must catch.
MUTANTS = {
    'a refusal does not ask for a lingering close (#1460)': [
        ("       /\\ linger' = (linger \\/ k = \"refuse\")\n", "       /\\ linger' = linger\n")],
    'the payload is taken beneath TLS (#1600)': [
        ('    /\\ trip = "none"\n    /\\ cell \\in Payloads\n', '    /\\ trip = "none"\n    /\\ FALSE\n'),
        ('    /\\ trip = "none"\n    /\\ cell \\notin Payloads\n', '    /\\ trip = "none"\n')],
}


def tla(value):
    if isinstance(value, bool):
        return 'TRUE' if value else 'FALSE'
    if isinstance(value, int):
        return str(value)
    if isinstance(value, list):
        return '<<' + ', '.join(tla(v) for v in value) + '>>'
    return json.dumps(value)


def labels(path):
    """(start, [(action, arg, layer)]) of a trace file."""
    with open(path) as f:
        lines = [json.loads(line) for line in f if line.strip()]
    if not lines or lines[0].get('point') != 'Start':
        raise ValueError(f'{path}: no Start line')
    steps = []
    for line in lines[1:]:
        name = line['point']
        if name in NO_STEP:
            continue
        steps.append((name, line.get('kind', ''), line.get('layer', '')))
    return lines[0], steps


def check(jar, path, spec=None):
    start, steps = labels(path)
    recs = ', '.join(f'[action |-> "{a}", arg |-> "{b}", layer |-> "{c}"]' for a, b, c in steps)
    kinds = start['kinds']
    trace = f'''---- MODULE Trace ----
EXTENDS Sequences
Trace == <<{recs}>>
TScript == {tla(kinds)}
THttps == {tla(start['transport'] == 'https')}
TVersion == {tla(start['version'])}
TTiming == {tla(start['timing'])}
TPipelines == {tla(start['pipelines'])}
====
'''
    kind_set = ', '.join(sorted({json.dumps(k) for k in kinds}))
    cfg = f"""CONSTANTS
    MaxReqs = {len(kinds)}
    Kinds = {{{kind_set}}}
    Transports = {{{json.dumps(start['transport'])}}}
    ClientVersions = {{{json.dumps(start['version'])}}}
    StubTimings = {{{json.dumps(start['timing'])}}}
    ClientPipelines = {{{tla(start['pipelines'])}}}
    BodyUnits = {start['bodyUnits']}
    PipelineFlush = FALSE
    ImposterArmsOnH2 = FALSE
    DrainOutrunsDelivery = TRUE
INIT TInit
NEXT TraceNext
INVARIANT TraceNotDone
"""
    extra = {'FaultIo': spec or read(os.path.join(here, 'FaultIo.tla')), 'Trace': trace}
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
    if sys.argv[2] == '--mutants':
        paths = collect(sys.argv[3:])
        spec = read(os.path.join(here, 'FaultIo.tla'))
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
            print(f'  {[a for a, _, _ in steps]}\n  {why}')
    sys.exit(1 if rejected or not paths else 0)


if __name__ == '__main__':
    main()
