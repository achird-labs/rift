#!/usr/bin/env python3
"""Put each bug the fault code excludes into the model and check that TLC reports the named invariant
violated.  Model mutation, not cargo-mutants (.github/workflows/mutants.yml).

Usage: python3 mutants.py <path to tla2tools.jar>.  Exits non-zero when a mutant goes uncaught or
is caught by anything but its expected invariant.  Each mutant is a textual edit of FaultIo.tla that
must still apply exactly once; a refactor of the model updates it.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc, without_invariant  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'FaultIo.tla'))
base_cfg = read(os.path.join(here, 'Mutants.cfg'))
jar = jar_from_argv()

TAKE_GUARD = '    /\\ trip = "none"\n    /\\ cell \\in Payloads\n'
SEAL_GUARD = '    /\\ trip = "none"\n    /\\ cell \\notin Payloads\n'
TAKE_PAYLOAD = "    /\\ cell' = None\n    /\\ trip' = \"writing\"\n"
TAKE_UNCHANGED = '''                   writing, wbuf, tlsBuf, linger, phase, soLinger, sendq, wire, unread, bodyLeft,
                   served, streams>>

\\* Otherwise the bytes pass through'''
LINGER_FIN = '''    /\\ linger
    /\\ sendq' = Append(sendq, <<"fin", 0>>)
    /\\ phase' = "draining"
    /\\ UNCHANGED <<script, https, version, timing, pipelines, hasStub, sent, current, svc, reading,
                   writing, wbuf, tlsBuf, cell, linger, trip, soLinger, wire, unread, bodyLeft,'''
ARM_LINGER = "       /\\ linger' = (linger \\/ k = \"refuse\")\n"
HTTP_TRIP = '''       ELSE /\\ TripRaw(cell)
            /\\ TripGhost(cell)
            /\\ UNCHANGED wbuf
'''
# name: (expected verdict, spec edits[, cfg edits])
mutants = {
    # #1600: a payload fault was written beneath TLS, by FaultIo, as raw bytes.
    'the payload is taken beneath TLS (#1600)': ('PayloadAboveTlsOnHttps', [
        (TAKE_GUARD, '    /\\ trip = "none"\n    /\\ FALSE\n'),
        (SEAL_GUARD, '    /\\ trip = "none"\n')]),
    # #1460's fix without its first half: the drain starts with no FIN sent.
    'the linger drains without shutting the write side': ('LingerDeliversResponse', [
        (LINGER_FIN, LINGER_FIN.replace("/\\ sendq' = Append(sendq, <<\"fin\", 0>>)\n    ", '').replace(
            'trip, soLinger, wire,', 'trip, soLinger, sendq, wire,'))]),
    # #1460 itself: a refusal closes at once, with the body unread.
    'a refusal does not ask for a lingering close (#1460)': ('LingerDeliversResponse', [
        (ARM_LINGER, "       /\\ linger' = linger\n")]),
    # `take` as a read above TLS: the cell stays armed, and FaultIo beneath fires it again.
    'take_payload reads instead of taking': ('FaultFiresAtMostOnce', [
        (TAKE_PAYLOAD, "    /\\ trip' = \"writing\"\n"),
        (TAKE_UNCHANGED, TAKE_UNCHANGED.replace('writing, wbuf, tlsBuf, linger,', 'writing, wbuf, tlsBuf, cell, linger,'))]),
    # The fault applied after forwarding hyper's write, instead of in its place.
    'the write goes out before the fault': ('FaultBeforeResponseBytes', [
        (HTTP_TRIP, """       ELSE /\\ cell' = None
            /\\ phase' = "aborted"
            /\\ soLinger' = (cell = "reset")
            /\\ sendq' = Append(sendq, Head(wbuf))
            /\\ TripGhost(cell)
            /\\ UNCHANGED wbuf
""")]),
    # hyper reading the next head before the last response left its buffer.  Over http and with no
    # refusal only: the earlier bytes in rustls also make FaultIo the first layer to see a payload
    # kind, and a fault ahead of a refusal resets it away, which the other two invariants report too,
    # and rightly.
    'hyper reads the next head with the last response unflushed': ('FaultNeverTripsEarlierBytes', [
        ('    /\\ (wbuf = <<>> /\\ tlsBuf = <<>>) \\/ FlushSkipped\n', '    /\\ TRUE\n')],
        [('Transports = {"http", "https"}', 'Transports = {"http"}'),
         ('"ok", "refuse", "reset"', '"ok", "reset"')]),
}
failed = False
# #1782: the imposter listener armed the connection's cell on HTTP/2, so one stream's fault aborted
# every stream on it.  Mutants.cfg has no stub write under an open connection, so this one takes the
# PR bounds.
verdict, trace, out = tlc(jar, 'FaultIo', spec, edit(read(os.path.join(here, 'FaultIoPR.cfg')), [
    ('ImposterArmsOnH2 = FALSE', 'ImposterArmsOnH2 = TRUE')], 'FaultIoPR.cfg'))
if verdict == 'H2FaultStaysOnStream':
    print(f'the imposter listener arms on HTTP/2 (#1782): caught by {verdict}')
else:
    print(f'the imposter listener arms on HTTP/2 (#1782): expected H2FaultStaysOnStream, got {verdict}\n{out}')
    failed = True
for name, (expected, spec_edits, *cfg_edits) in mutants.items():
    mutated = edit(spec, spec_edits, name)
    cfg = edit(base_cfg, cfg_edits[0] if cfg_edits else [], name)
    verdict, trace, out = tlc(jar, 'FaultIo', mutated, cfg)
    # Caught by that invariant only: without it, the same mutant passes every other one.
    rest, _, rest_out = tlc(jar, 'FaultIo', mutated, without_invariant(cfg, expected))
    if verdict == expected and rest == 'ok':
        print(f'{name}: caught by {verdict} only')
    elif verdict == expected:
        print(f'{name}: also violates {rest}\n{rest_out}')
        failed = True
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
