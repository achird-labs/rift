#!/usr/bin/env python3
"""Put each fixed or excluded state cell bug back into the model and check that TLC reports the named
invariant violated.  Model mutation, not cargo-mutants.

Usage: python3 mutants.py <path to tla2tools.jar>.  Exits non-zero when a mutant goes uncaught or is caught
by anything but its expected invariant.  Each mutant is a textual edit of StateCell.tla that must still apply
exactly once; a refactor of the model updates it.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'StateCell.tla'))
cfg = read(os.path.join(here, 'StateCellPR.cfg'))
jar = jar_from_argv()

NEXT = '''Next ==
    \\/ \\E r \\in Runs :
         \\/ Capture(r) \\/ Submit(r) \\/ CallerLeaves(r)
'''
SAVE_GUARD = '''EndSave(r) ==
    /\\ run[r].pc = "save"
'''
SAVE_NEXT = '''    /\\ run' = [run EXCEPT ![r].pc = "finish"]
'''
ANSWER_GUARD = '''FinishAnswered(r) ==
    /\\ run[r].pc = "finish"
'''
ANSWER_NEXT = '''run' = [run EXCEPT ![r].pc = "release", ![r].answered'''
ADD_ACTION = lambda action, name: [
    (NEXT, f'{action}\n\n{NEXT}\n'.rstrip('\n') + f'\n         \\/ {name}(r)\n'),
]

# name: (expected verdict, spec edits, cfg edits)
mutants = {
    # #1786: a left-behind job's throw ended the run that inherited it.
    'an inherited job\'s throw ends the heir (#1786)': ('HeirFailsOnlyByItsOwnJob', [
        ("""    /\\ run' = [run EXCEPT ![r].inherited = @ - 1, ![r].seq = @ + 1]
    /\\ UNCHANGED <<busy, heirFailed>>
""", """    /\\ \\/ /\\ run' = [run EXCEPT ![r].inherited = @ - 1, ![r].seq = @ + 1]
          /\\ UNCHANGED <<busy, heirFailed>>
       \\/ /\\ run' = [run EXCEPT ![r].inherited = @ - 1, ![r].seq = @ + 1, ![r].pc = "save",
                                ![r].end = "threw"]
          /\\ busy' = [busy EXCEPT ![run[r].worker] = r]
          /\\ heirFailed' = heirFailed \\cup {r}
""")], []),
    # #1678's first cut: the answer went out, then the state was saved.
    'the answer goes out before the save (#1678)': ('CellCurrentBeforeAnswer', [
        (SAVE_GUARD, SAVE_GUARD.replace('"save"', '"finish"')),
        (SAVE_NEXT, SAVE_NEXT.replace('"finish"', '"release"')),
        (ANSWER_GUARD, ANSWER_GUARD.replace('"finish"', '"save"')),
        (ANSWER_NEXT, ANSWER_NEXT.replace('"release"', '"finish"'))], []),
    # #1726: the state belonged to the port, so a run across a restart wrote into its successor's.
    'a run saves to the imposter\'s current cell (#1726)': ('OldRunSavesToOldCell', [
        ("cell' = [cell EXCEPT ![c] = live[k]]", "cell' = [cell EXCEPT ![current] = live[k]]")], []),
    # MbRealm::end's "the run's jobs go before its claim", and the save before both.
    'the claim is released before the save': ('OneLiveStatePerKey', ADD_ACTION(
        '''EarlyRelease(r) ==
    /\\ run[r].pc = "save"
    /\\ r \\in holding
    /\\ Released(r)
    /\\ UNCHANGED <<current, access, accessChanges, cell, run, live, present, busy, panics,
                   written, owed, savedToOld, heirFailed>>''', 'EarlyRelease') + [
        (SAVE_GUARD, SAVE_GUARD + '    /\\ r \\notin holding\n')], []),
    # MbJsThread::split_from: two realms would each hold a live state.
    'no deferral across realms': ('OneLiveStatePerKey', [
        ('SplitFrom(r) == \\E a \\in Accesses \\ {run[r].files} : <<run[r].worker, a, run[r].cell>> \\in present',
         'SplitFrom(r) == FALSE')], []),
    # MbRealm::save_state's in_flight check: the live state dropped under a run still using it (#477).
    'the live state goes with any run\'s end': ('NoLostUpdate', [
        ('last == Kin(r) = {}', 'last == TRUE')], []),
    # MbPool::claim: the route is ignored.
    'a run goes to any worker': ('NeverTwoWorkersOneKey', [
        ('\\E w \\in IF owner[c] = NoWorker THEN Workers ELSE {owner[c]} :', '\\E w \\in Workers :')], []),
    # MbRun's Drop: a job runs after its run's last save.
    'a run\'s jobs outlive it': ('NoLostUpdate', [
        ('ended == [run EXCEPT ![r].pc = "done", ![r].own = 0, ![r].inherited = 0]',
         'ended == [run EXCEPT ![r].pc = "done"]'),
        ('''OwnJob(r) ==
    /\\ run[r].pc = "flight"''', '''OwnJob(r) ==
    /\\ run[r].pc \\in {"flight", "done"}''')],
        [('    EndedRunsLeaveNothing\n', '')]),
    # What a reentrant run_blocking on a pool worker would do (js_engine run_blocking): a fresh MbJsThread,
    # seeded from the cell and saved to it, beside the imposter's live state.  README.md, "Refuted": no
    # code path reaches it; this says what it would cost if one did.
    'a run re-enters on a fresh MbJsThread': ('NoLostUpdate', ADD_ACTION(
        '''Reenter(r) ==
    /\\ run[r].pc = "flight"
    /\\ cell' = [cell EXCEPT ![run[r].cell] = @ \\cup {Tok(r)}]
    /\\ written' = [written EXCEPT ![run[r].cell] = @ \\cup {Tok(r)}]
    /\\ run' = [run EXCEPT ![r].seq = @ + 1]
    /\\ UNCHANGED <<current, access, accessChanges, owner, holding, live, present, busy, panics,
                   owed, savedToOld, heirFailed>>''', 'Reenter'), []),
}
failed = False
for name, (expected, spec_edits, cfg_edits) in mutants.items():
    verdict, trace, out = tlc(jar, 'StateCell', edit(spec, spec_edits, name), edit(cfg, cfg_edits, name),
                              deadlock=False)
    if verdict == expected:
        print(f'{name}: caught by {verdict}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
