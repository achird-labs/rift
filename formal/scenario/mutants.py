#!/usr/bin/env python3
"""Put each fixed scenario bug back into the model and check that TLC reports its named invariant
violated, and no other.  Model mutation, not cargo-mutants (.github/workflows/mutants.yml).

Usage: python3 mutants.py <path to tla2tools.jar>.  Exits non-zero when a mutant goes uncaught, or
when it also violates another invariant of the gate.  Each mutant is a textual edit of Scenario.tla
that must still apply exactly once; a refactor of the model updates it.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc, without_invariant  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'Scenario.tla'))
cfg = read(os.path.join(here, 'Mutants.cfg'))
jar = jar_from_argv()

MATCHED = '![r].pc = IF r \\in Gated THEN "cas" ELSE "serve"]'

# name: (expected invariant, spec edits)
mutants = {
    # #1307: the stamp went out before the store write, so a request could copy it, read the old
    # state and serve the old answer under it.  Issue records the write it is about to make.
    'stamp before the store write (#1307)': ('NeverOldAnswerUnderNewStamp', [
        (MATCHED, '![r].pc = IF r \\in Gated THEN "issue" ELSE "serve"]'),
        ('writes \\cup {[ver |-> req[r].wrote,', 'writes \\cup {[ver |-> ver + 1,'),
        ('[@ EXCEPT !.pc = "serve"]', '[@ EXCEPT !.pc = "cas"]'),
        ('![r].wrote = ver + 1, ![r].pc = "issue"]', '![r].wrote = ver + 1, ![r].pc = "serve"]')]),
    # #311: a gated transition wrote with a plain `set`, over whatever had moved since its gate.
    'a gated transition is a plain set (#311)': ('TransitionsAreCas', [
        ('/\\ IF store = req[r].exp\n', '/\\ IF TRUE\n')]),
    # #1307: the copy came after the gates, so a transition landing between them stamped the answer
    # it had replaced.
    'the stamps are copied after the gate read (#1307)': ('NeverOldAnswerUnderNewStamp', [
        ('IF r \\in Gated \\cup Pollers THEN "copy"', 'IF r \\in Gated \\cup Pollers THEN "gate"'),
        (MATCHED, '![r].pc = "copy"]'),
        ('![r].copy = map, ![r].pc = "gate"]', '![r].copy = map, ' + MATCHED)]),
}
failed = False
for name, (expected, spec_edits) in mutants.items():
    mutated = edit(spec, spec_edits, name)
    verdict, trace, out = tlc(jar, 'Scenario', mutated, cfg)
    others, _, others_out = tlc(jar, 'Scenario', mutated, without_invariant(cfg, expected))
    if verdict == expected and others == 'ok':
        print(f'{name}: caught by {verdict} only')
    else:
        print(f'{name}: expected {expected} and nothing else, got {verdict}, then {others}\n'
              f'  {" -> ".join(trace)}\n{out}\n{others_out}')
        failed = True
sys.exit(1 if failed else 0)
