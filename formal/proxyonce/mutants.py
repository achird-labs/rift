#!/usr/bin/env python3
"""Put each fixed proxyOnce bug back into the model and check that TLC reports the named invariant
violated.  Model mutation, not cargo-mutants (.github/workflows/mutants.yml).

Usage: python3 mutants.py <path to tla2tools.jar>.  Exits non-zero when a mutant goes uncaught or is
caught by anything but its expected invariant.  Each mutant is a textual edit of ProxyOnce.tla that
must still apply exactly once; a refactor of the model updates it.  The cfg is ProxyOncePR.cfg's, so
every green invariant is checked at once and the named one has to be the first to fall.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'ProxyOnce.tla'))
cfg = read(os.path.join(here, 'ProxyOncePR.cfg'))
jar = jar_from_argv()

DROP_RELEASE = """    /\\ req[r].pc \\in {"forward", "settle"}
    /\\ pending' = IF req[r].token # NoToken /\\ pending = req[r].token THEN NoToken ELSE pending
"""
EDIT_FORGET = """    /\\ edit[e].pc = "forget"
    /\\ responses' = NoRec
    /\\ ghost' = ForgetGhost(edit[e].saw)
"""
SHADOW_CHECK = 'IF Shadowed(stubs, req[r].answering) THEN "forget" ELSE "lookup"'
SETTLE_GUARD = 'LET lands == req[r].token # NoToken /\\ pending = req[r].token'
SETTLE_FAIL_RELEASE = """    /\\ req' = [req EXCEPT ![r].pc = AfterSettle(r)]
    /\\ pending' = IF pending = req[r].token THEN NoToken ELSE pending
    /\\ UNCHANGED <<responses, nextToken,"""
AFTER_SETTLE = 'AfterSettle(r) == IF GeneratesStub /\\ ~req[r].lost THEN "insert" ELSE "done"'
# #171/#118: the check of `responses` is its own step, so a record can land before the claim.  The
# request reads "not recorded" at "claim", and takes the claim at "claimLate".
SPLIT_READ = """Claim(r) ==
    /\\ req[r].pc = "claim"
    /\\ TryClaim(r, IF GeneratesStub THEN "shadow" ELSE "lookup")
"""
SPLIT_READ_NEW = """Claim(r) ==
    \\/ /\\ req[r].pc = "claim"
       /\\ responses = NoRec
       /\\ req' = [req EXCEPT ![r].pc = "claimLate"]
       /\\ UNCHANGED <<responses, pending, nextToken, stubs, nextSlot, edit, clear, restart, ghost>>
    \\/ /\\ req[r].pc = "claim"
       /\\ responses # NoRec
       /\\ TryClaim(r, IF GeneratesStub THEN "shadow" ELSE "lookup")
    \\/ /\\ req[r].pc = "claimLate"
       /\\ IF pending # NoToken
          THEN /\\ req' = [req EXCEPT ![r].pc = "forward", ![r].lost = TRUE]
               /\\ UNCHANGED <<pending, nextToken>>
          ELSE /\\ pending' = nextToken
               /\\ nextToken' = nextToken + 1
               /\\ req' = [req EXCEPT ![r].pc = "forward", ![r].token = nextToken]
       /\\ UNCHANGED <<responses, stubs, nextSlot, edit, clear, restart, ghost>>
"""
PCS = 'Pcs == {"idle", "claim",'

# name: (expected verdict, spec edits)
mutants = {
    # #1193: a dropped future left its claim held for the life of the imposter.
    'a dropped request keeps its claim (#1193)': ('NoWedgedClaim', [
        (DROP_RELEASE, '    /\\ req[r].pc \\in {"forward", "settle"}\n    /\\ UNCHANGED pending\n')]),
    # #1386: a recording outlived its stub, and kept replaying after the stub was deleted.
    'an edit does not forget the orphaned claim (#1386)': ('ClaimLifetimeIsStubLifetime', [
        (EDIT_FORGET, '    /\\ edit[e].pc = "forget"\n    /\\ UNCHANGED responses\n    /\\ UNCHANGED ghost\n')]),
    # #1705: a recording behind the answering stub replayed instead of being forwarded past.
    'no shadow check (#1705)': ('ShadowedRecordingForwards', [(SHADOW_CHECK, '"lookup"')]),
    # #171/#118: `responses` read outside the claim, so a recording lands between the read and the
    # claim, and the request claims a signature that is already recorded.
    'try_claim reads responses and claims in two steps (#171/#118)': ('ClaimExcludesRecording', [
        (SPLIT_READ, SPLIT_READ_NEW), (PCS, 'Pcs == {"idle", "claim", "claimLate",')]),
    # `record` trusted any token: a request from before a clear records over the current claim.
    'record does not check the token': ('RecordedOnce', [
        (SETTLE_GUARD, 'LET lands == req[r].token # NoToken')]),
    # A failed settle kept the claim: the signature wedged as InFlight (#315, #910).
    'a failed settle keeps the claim (#315)': ('NoWedgedClaim', [
        (SETTLE_FAIL_RELEASE, SETTLE_FAIL_RELEASE.replace(
            "    /\\ pending' = IF pending = req[r].token THEN NoToken ELSE pending\n"
            "    /\\ UNCHANGED <<responses, nextToken,",
            "    /\\ UNCHANGED <<pending, responses, nextToken,"))]),
    # A loser inserted its stub too, one duplicate per loser.
    # #1778: an empty lookup forwarded with no claim and inserted a stub carrying none. The forward
    # is the earlier symptom, a step before the insert OnlyClaimHolderInserts catches.
    'an empty lookup forwards with no claim and is not lost': ('ForwardUnrecordedOnlyWhenLost', [
        ('''req' = [req EXCEPT ![r].pc = "reclaim", ![r].again = TRUE]''', '''req' = [req EXCEPT ![r].pc = "forward"]''')]),
    'a loser inserts its stub': ('LoserNeverRecords', [
        (AFTER_SETTLE, 'AfterSettle(r) == IF GeneratesStub THEN "insert" ELSE "done"')]),
}
failed = False
for name, (expected, spec_edits) in mutants.items():
    verdict, trace, out = tlc(jar, 'ProxyOnce', edit(spec, spec_edits, name), cfg)
    if verdict == expected:
        print(f'{name}: caught by {verdict}\n  {" -> ".join(trace)}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
