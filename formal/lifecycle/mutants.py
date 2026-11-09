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
    /\\ op' = [op EXCEPT ![o].pc = "dClear"]'''
COMPUTE_GUARD = '''ReqCompute(g) ==
    /\\ phase[g] \\in Serving
    /\\ req[g] = "computing"'''
LOOKUP_KEEPS_LOCK = '''            ELSE /\\ op' = [op EXCEPT ![o].pc = "dUnlink", ![o].look = slot[p]]
                 /\\ UNCHANGED persistLock'''
REMOVE_IF_SAME = '       IN IF slot[p] = op[o].look\n            THEN /\\ ghost\' = [ghost EXCEPT !.removedOther'
UNLINK_OK = '''       IN \\/ /\\ file' = [file EXCEPT ![p] = NoGen]
             /\\ op' = [op EXCEPT ![o].pc = "dCancel"]
             /\\ UNCHANGED <<slot, persistLock, ghost>>
          \\/ /\\ UnlinkFails
             /\\ persistLock' = NoOp
             /\\ UNCHANGED <<slot, file>>'''
FLOOR_READ = "           f == floor[op[o].port]\n"
FLOOR_REREAD = "           again == floor[op[o].port]\n"
PERSIST_FAIL = '''                 \\/ /\\ PersistFails
                    /\\ op' = [op EXCEPT ![o].pc = "cRollback"]'''
CLAIM_ATOMIC = '''            THEN /\\ slot' = [slot EXCEPT ![p] = g]
                 /\\ phase' = [phase EXCEPT ![g] = "admitting"]
                 /\\ op' = [op EXCEPT ![o].pc = "cReread"]
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
        (REMOVE_IF_SAME, REMOVE_IF_SAME.replace('IF slot[p] = op[o].look', 'IF slot[p] # NoGen'))],
        [('    SharedStateBelongsToCurrent\n', '')]),
    # #1124: the imposter left the table before its file went, so a failed unlink left it unregistered.
    'the file goes after the imposter (#1124)': ('ErrDeleteStillServing', [
        (UNLINK_OK, UNLINK_OK.replace("/\\ UNCHANGED <<slot, file>>", "/\\ slot' = [slot EXCEPT ![p] = NoGen]\n             /\\ UNCHANGED file"))],
        [('UnlinkFails = FALSE', 'UnlinkFails = TRUE')]),
    # #1301: a new generation started its load clock from zero, below the stamps the deleted one served.
    'the load floor is not carried to the next generation (#1301)': ('FloorMonotone', [
        (FLOOR_READ, "           f == 0\n"), (FLOOR_REREAD, "           again == 0\n")], []),
    # #1763: the shared state was cleared after the slot was vacated, when it could be the next
    # generation's.
    'the clear runs after the slot is vacated (#1763)': ('SharedStateBelongsToCurrent', [
        (CANCEL_CUT, CANCEL_CUT.replace('"dClear"', '"dFloor"')),
        ('''          /\\ op' = [op EXCEPT ![o].pc = "dFloor"]
    /\\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,''',
         '''          /\\ op' = [op EXCEPT ![o].pc = "dDrain"]
    /\\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,'''),
        ('''                 /\\ op' = [op EXCEPT ![o].pc = "dDrain"]''', '''                 /\\ op' = [op EXCEPT ![o].pc = "dClear"]''')], []),
    # #1762: the floor was recorded before the cancel, so a stamp could still land above it.
    'the floor is recorded before the cancel (#1762)': ('FloorMonotone', [
        (UNLINK_OK, UNLINK_OK.replace('"dCancel"', '"dFloor"')),
        ('''    /\\ op' = [op EXCEPT ![o].pc = "dRemove"]''', '''    /\\ op' = [op EXCEPT ![o].pc = "dCancel"]'''),
        ('''          /\\ op' = [op EXCEPT ![o].pc = "dFloor"]
    /\\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,''',
         '''          /\\ op' = [op EXCEPT ![o].pc = "dRemove"]
    /\\ UNCHANGED <<slot, file, floor, nextGen, persistLock, phase, clock, nreq, req,''')], []),
    # #563: a failed persist returned Err with the imposter still registered and serving.
    'a failed persist leaves the imposter running (#563)': ('ErrCreateNotRunning', [
        (PERSIST_FAIL, PERSIST_FAIL.replace('![o].pc = "cRollback"', '![o].pc = op[o].ret, ![o].fail = "persist"'))],
        [('PersistFails = FALSE', 'PersistFails = TRUE')]),
    # #1773: a failed create's rollback removed whichever generation held the port.
    'the rollback removes whoever holds the port (#1773)': ('RemoveOnlyOwn', [
        ("IN /\\ ghost' = ghost\n", "IN /\\ ghost' = [ghost EXCEPT !.removedOther = @ \\/ (slot[p] # NoGen /\\ slot[p] # g)]\n"),
        ("/\\ slot' = IF slot[p] = g THEN [slot EXCEPT ![p] = NoGen] ELSE slot", "/\\ slot' = [slot EXCEPT ![p] = NoGen]")],
        [('Creators = {c1}', 'Creators = {c1, c2}'), ('Preloaded = TRUE', 'Preloaded = FALSE'),
         ('PersistFails = FALSE', 'PersistFails = TRUE')]),
    # #1773: the rollback waits for no lock, so it can take the imposter out from under a delete that
    # has looked it up, whose failed unlink then says "still serving" of an imposter already gone.
    'the rollback does not take persist_lock (#1773)': ('ErrDeleteStillServing', [
        ("""    /\\ op[o].pc = "cRollback"
    /\\ persistLock = NoOp
""", """    /\\ op[o].pc = "cRollback"
""")],
        [('PersistFails = FALSE', 'PersistFails = TRUE'), ('UnlinkFails = FALSE', 'UnlinkFails = TRUE')]),
    # #1774: an apply sweep reported a port a concurrent delete had removed as failed.
    'an apply sweep reports a concurrent delete as failed (#1774)': ('ApplyReportsTruth', [
        ('op[o].ret \\notin {"rCreate", "aSweep"}', 'op[o].ret # "rCreate"')],
        [('Appliers = {}', 'Appliers = {a1}'), ('Deleters = {d1, d2}', 'Deleters = {d1}')]),
    # #1803: a stub write or a restart looked the imposter up and cleared by port with no lock, so a
    # delete and re-create could run in between and its clear wiped the next generation's state.
    'a stub write clears without the lock (#1803)': ('SharedStateBelongsToCurrent', [
        ('[] KindOf(o) = "write"     -> "wLock"', '[] KindOf(o) = "write"     -> "wLookup"'),
        ('''            THEN /\\ persistLock' = NoOp
                 /\\ op' = [op EXCEPT ![o].pc = "done"]''', '''            THEN /\\ UNCHANGED persistLock
                 /\\ op' = [op EXCEPT ![o].pc = "done"]'''),
        ('''    /\\ persistLock' = NoOp
    /\\ op' = [op EXCEPT ![o].pc = "done"]''', '''    /\\ UNCHANGED persistLock
    /\\ op' = [op EXCEPT ![o].pc = "done"]''')],
        [('Writers = {}', 'Writers = {w1}'), ('Deleters = {d1, d2}', 'Deleters = {d1}')]),
    # No `mutations` lock: two creates see the slot empty and both store.
    'try_claim without the mutations lock': ('OneClaimant', [
        (CLAIM_ATOMIC, '''            THEN /\\ op' = [op EXCEPT ![o].pc = "cStore"]
                 /\\ UNCHANGED <<slot, phase, req, ghost>>''')],
        [('Creators = {c1}', 'Creators = {c1, c2}'), ('Preloaded = TRUE', 'Preloaded = FALSE')]),
    # #1762: a create that read the floor before a delete recorded it claimed the freed slot with a
    # clock that never saw it.
    'the floor is not re-read after the claim (#1762)': ('FloorMonotone', [
        (FLOOR_REREAD, "           again == 0\n")],
        [('Creators = {c1}', 'Creators = {c1, c2}'), ('Preloaded = TRUE', 'Preloaded = FALSE')]),
    # #1762: the gate opened before the re-read, so a request was answered under the old stamp.
    'the gate opens before the floor is re-read (#1762)': ('FloorMonotone', [
        ('Serving == {"claimed"}', 'Serving == {"admitting", "claimed"}')],
        [('Creators = {c1}', 'Creators = {c1, c2}'), ('Preloaded = TRUE', 'Preloaded = FALSE')]),
    # #1762: a rollback dropped the stamps its generation served after the claim.
    'a rollback records no floor (#1762)': ('FloorMonotone', [
        ("          /\\ floor' = [floor EXCEPT ![p] = Max(@, clock[g].last)]\n", ''),
        ('''    /\\ UNCHANGED <<nextGen, persistLock, clock, nreq, deleteReturned,
''', '''    /\\ UNCHANGED <<floor, nextGen, persistLock, clock, nreq, deleteReturned,
''')],
        [('Creators = {c1}', 'Creators = {c1, c2}'), ('Preloaded = TRUE', 'Preloaded = FALSE'),
         ('PersistFails = FALSE', 'PersistFails = TRUE')]),
    # #1764: the accept loops served as soon as they were spawned, before the claim.
    'the accept loops answer before the claim (#1764)': ('LoserNeverAnswers', [
        ('Serving == {"claimed"}', 'Serving == {"accepting", "claimed"}')],
        [('Creators = {c1}', 'Creators = {c1, c2}'), ('Preloaded = TRUE', 'Preloaded = FALSE')]),
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
# #1800: a failed create's rollback left the file a replace that lost the port to it had kept, so a
# restart brought the replaced imposter back.  Needs a replace and failing writes together.
ROLLBACK_FILE = "          /\\ file' = IF slot'[p] = NoGen THEN [file EXCEPT ![p] = NoGen] ELSE file\n"
name = "a failed create's rollback leaves a replaced imposter's file (#1800)"
verdict, trace, out = tlc(jar, 'Lifecycle', edit(spec, [
    (ROLLBACK_FILE, ''),
    ('''    /\\ UNCHANGED <<nextGen, persistLock, clock, nreq, deleteReturned,
                   lastStamp, shared>>''', '''    /\\ UNCHANGED <<file, nextGen, persistLock, clock, nreq, deleteReturned,
                   lastStamp, shared>>''')], name), read(os.path.join(here, 'LifecycleReplaceFail.cfg')))
if verdict == 'TableFileAgree':
    print(f'{name}: caught by {verdict}')
else:
    print(f'{name}: expected TableFileAgree, got {verdict}\n  {" -> ".join(trace)}\n{out}')
    failed = True
for name, (expected, spec_edits, cfg_edits) in mutants.items():
    verdict, trace, out = tlc(jar, 'Lifecycle', edit(spec, spec_edits, name), edit(cfg, cfg_edits, name))
    if verdict == expected:
        print(f'{name}: caught by {verdict}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
