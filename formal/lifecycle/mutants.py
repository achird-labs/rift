#!/usr/bin/env python3
"""Put each fixed lifecycle bug back into the model and check that TLC reports the named invariant
violated.  Model mutation, not cargo-mutants (.github/workflows/mutants.yml).

Usage: python3 mutants.py <path to tla2tools.jar>.  Exits non-zero when a mutant goes uncaught or
is caught by anything but its expected invariant.  Each mutant is a textual edit of Lifecycle.tla
(and sometimes Mutants.cfg) that must still apply exactly once; a refactor of the model updates it.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'Lifecycle.tla'))
cfg = read(os.path.join(here, 'Mutants.cfg'))
jar = jar_from_argv()

DRAIN = '    /\\ req[op[o].look] # "writing" \\/ DrainTimesOut\n'
CANCEL_CUT = '''       IN /\\ phase' = [phase EXCEPT ![g] = "cancelled"]
          /\\ req' = [req EXCEPT ![g] = Cut(@)]
    /\\ op' = [op EXCEPT ![o].pc = "dDrain"]'''
COMPUTE_GUARD = '''ReqCompute(g) ==
    /\\ phase[g] \\in Serving
    /\\ req[g] = "computing"'''
LOOKUP_KEEPS_LOCK = '''            ELSE /\\ op' = [op EXCEPT ![o].pc = "dUnlink", ![o].look = slot[p]]
                 /\\ UNCHANGED persistLock'''
REMOVE_IF_SAME = '       IN IF slot[p] = op[o].look\n            THEN /\\ ghost\' = [ghost EXCEPT !.removedOther'
UNLINK_OK = '''       IN \\/ /\\ file' = [file EXCEPT ![p] = NoGen]
             /\\ op' = [op EXCEPT ![o].pc = "dFloor"]
             /\\ UNCHANGED <<slot, persistLock, ghost>>
          \\/ /\\ UnlinkFails
             /\\ persistLock' = NoOp
             /\\ UNCHANGED <<slot, file>>'''
FLOOR_READ = "           f == floor[op[o].port]\n"
PERSIST_FAIL = '''                 \\/ /\\ PersistFails
                    /\\ op' = [op EXCEPT ![o].pc = "cRollback"]'''
FLOOR_RECORD = "    /\\ floor' = [floor EXCEPT ![op[o].port] = Max(@, clock[op[o].look].last)]\n"
CANCEL_TAIL = '''    /\\ op' = [op EXCEPT ![o].pc = "dDrain"]
    /\\ UNCHANGED <<slot, file, floor, nextGen, persistLock, clock, nreq, deleteReturned,'''
# The proposed fix of the one-create floor race: record the floor once the cancel has cut every
# request that could still stamp, not before the removal.
FLOOR_FIXED = [
    (FLOOR_RECORD, "    /\\ UNCHANGED floor\n"),
    (CANCEL_TAIL, CANCEL_TAIL.replace(
        '    /\\ UNCHANGED <<slot, file, floor, nextGen,',
        "    /\\ floor' = [floor EXCEPT ![op[o].port] = Max(@, clock[op[o].look].last)]\n"
        '    /\\ UNCHANGED <<slot, file, nextGen,')),
]
CLAIM_ATOMIC = '''            THEN /\\ slot' = [slot EXCEPT ![p] = g]
                 /\\ phase' = [phase EXCEPT ![g] = "claimed"]
                 /\\ op' = [op EXCEPT ![o].pc = "cPersist"]
                 /\\ UNCHANGED <<req, ghost>>'''

# name: (expected verdict, spec edits, cfg edits)
mutants = {
    # #596: the delete returned once the listener was signalled, without draining the connections.
    'delete returns before the drain (#596)': ('NoServeAfterDeleteReturns', [(DRAIN, '    /\\ TRUE\n')], []),
    # #1643: the cancel did not cut requests still computing, so one could answer after the delete.
    'cancel leaves computing requests running (#1643)': ('NoServeAfterDeleteReturns', [
        (CANCEL_CUT, CANCEL_CUT.replace('Cut(@)', '@')),
        (COMPUTE_GUARD, COMPUTE_GUARD.replace('phase[g] \\in Serving', 'phase[g] # "unborn"'))], []),
    # #1124: the delete looked the imposter up, then removed whatever held the port, outside a lock.
    'delete removes whoever holds the port (#1124)': ('RemoveOnlyOwn', [
        (LOOKUP_KEEPS_LOCK, LOOKUP_KEEPS_LOCK.replace('UNCHANGED persistLock', "persistLock' = NoOp")),
        (REMOVE_IF_SAME, REMOVE_IF_SAME.replace('IF slot[p] = op[o].look', 'IF slot[p] # NoGen'))], []),
    # #1124: the imposter left the table before its file went, so a failed unlink left it unregistered.
    'the file goes after the imposter (#1124)': ('ErrDeleteStillServing', [
        (UNLINK_OK, UNLINK_OK.replace("/\\ UNCHANGED <<slot, file>>", "/\\ slot' = [slot EXCEPT ![p] = NoGen]\n             /\\ UNCHANGED file"))],
        [('UnlinkFails = FALSE', 'UnlinkFails = TRUE')]),
    # #1301: a new generation started its load clock from zero, below the stamps the deleted one served.
    # Checked on the model with the proposed fix of the one-create floor race applied (FLOOR_FIXED:
    # the floor is recorded at the cancel, when no further stamp can be issued), since the unfixed
    # model violates FloorMonotone on these bounds by itself (README.md, "Findings").
    'the load floor is not carried to the next generation (#1301)': ('FloorMonotone', FLOOR_FIXED + [
        (FLOOR_READ, "           f == 0\n")],
        [('    TypeInv\n', '    TypeInv\n    FloorMonotone\n')]),
    # #563: a failed persist returned Err with the imposter still registered and serving.
    'a failed persist leaves the imposter running (#563)': ('ErrCreateNotRunning', [
        (PERSIST_FAIL, PERSIST_FAIL.replace('![o].pc = "cRollback"', '![o].pc = op[o].ret, ![o].fail = "persist"'))],
        [('PersistFails = FALSE', 'PersistFails = TRUE')]),
    # No `mutations` lock: two creates see the slot empty and both store.
    'try_claim without the mutations lock': ('OneClaimant', [
        (CLAIM_ATOMIC, '''            THEN /\\ op' = [op EXCEPT ![o].pc = "cStore"]
                 /\\ UNCHANGED <<slot, phase, req, ghost>>''')],
        [('Creators = {c1}', 'Creators = {c1, c2}'), ('Preloaded = TRUE', 'Preloaded = FALSE'),
         ('    LoserNeverAnswers\n', '')]),
}
failed = False
# A drain that never ends: the delete cannot return, so progress fails under fairness.
verdict, trace, out = tlc(jar, 'Lifecycle', edit(spec, [(DRAIN, '    /\\ FALSE\n')], 'stuck drain'),
                          read(os.path.join(here, 'Liveness.cfg')))
if verdict in ('OpsFinish', 'temporal'):
    print(f'a drain that never ends: caught by {verdict}')
else:
    print(f'a drain that never ends: expected OpsFinish, got {verdict}\n{out}')
    failed = True
for name, (expected, spec_edits, cfg_edits) in mutants.items():
    verdict, trace, out = tlc(jar, 'Lifecycle', edit(spec, spec_edits, name), edit(cfg, cfg_edits, name))
    if verdict == expected:
        print(f'{name}: caught by {verdict}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
