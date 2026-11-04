#!/usr/bin/env python3
"""Check that the fault model's documented limits, and the defects it found that are still open, stay
reachable: each case runs TLC with one change and requires exactly the expected verdict.

Usage: python3 witness.py <path to tla2tools.jar>.  A limit that became unreachable means the code
or the model changed, and README.md must change with it; a finding that became unreachable means
its fix landed, and its invariant moves back to FaultIoPR.cfg.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import edit, jar_from_argv, read, tlc  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
spec = read(os.path.join(here, 'FaultIo.tla'))
pr_cfg = read(os.path.join(here, 'FaultIoPR.cfg'))
known_cfg = read(os.path.join(here, 'Known.cfg'))
witness_cfg = read(os.path.join(here, 'Witness.cfg'))
jar = jar_from_argv()


def only(cfg, invariant):
    """`cfg` with `invariant` as its only invariant."""
    head = cfg.split('INVARIANTS')[0]
    return f'{head}INVARIANTS\n    {invariant}\n'


cases = [
    # The issue's witness: the ordering needs proving only because of the second response.
    ('a fault is armed after an earlier response went out (two requests)',
     only(witness_cfg, 'NeverArmAfterFirstWrite'), 'NeverArmAfterFirstWrite'),
    ('the same, with one request (control)',
     edit(only(witness_cfg, 'NeverArmAfterFirstWrite'), [('MaxReqs = 2', 'MaxReqs = 1')], 'Witness.cfg'), 'ok'),
    # The pipelining answer rests on hyper's flush gate, which this option turns off.
    ('with pipeline_flush a fault for N+1 trips on the bytes of N (io.rs:279)',
     edit(only(witness_cfg, 'FaultNeverTripsEarlierBytes'), [('PipelineFlush = FALSE', 'PipelineFlush = TRUE')],
          'Witness.cfg'), 'FaultNeverTripsEarlierBytes'),
    ('the same, without it, as Rift runs hyper (control)',
     only(witness_cfg, 'FaultNeverTripsEarlierBytes'), 'ok'),
    # Documented limits (README.md).
    ('bytes pipelined past a graceful fault turn its close into a reset',
     only(witness_cfg, 'GracefulFaultEndsInFin'), 'GracefulFaultEndsInFin'),
    ('a payload fault over http is lost when the socket is full (fault_io.rs try_write)',
     only(witness_cfg, 'PayloadAlwaysWritten'), 'PayloadAlwaysWritten'),
    ('a drain that outlasts delivery resets the refusal away',
     edit(only(witness_cfg, 'LingerDeliversResponse'), [('DrainOutrunsDelivery = FALSE', 'DrainOutrunsDelivery = TRUE')],
          'Witness.cfg'), 'LingerDeliversResponse'),
    # `FaultCell::take` as a read over plain http is an equivalent mutant: hyper never writes again
    # after the first error (dispatch.rs:451, :128).  The read matters above TLS (mutants.py).
    ('take as a read on the raw socket changes nothing, since hyper aborts on the first error',
     None, 'ok'),
    # The rule the other listeners apply (gateway.rs:38) keeps the fault on its stream: the fix.
    ('apply_tcp_fault\'s version check on the imposter listener (control for #1782)',
     edit(only(known_cfg, 'H2FaultStaysOnStream'), [('ImposterArmsOnH2 = TRUE', 'ImposterArmsOnH2 = FALSE')], 'Known.cfg'), 'ok'),
    # Filed findings, reachable until fixed (README.md, "Findings").
    ('one stream\'s fault takes the other streams of an HTTP/2 connection (#1782)',
     only(known_cfg, 'H2FaultStaysOnStream'), 'H2FaultStaysOnStream'),
    # Accepted as what a reset means (#1783, closed): it stays reachable, and documented.
    ('a close that sends RST discards what the kernel still holds',
     only(known_cfg, 'RstDiscardsNothing'), 'RstDiscardsNothing'),
    # The PR gate is green with #1782 in Known.cfg.
    ('FaultIoPR.cfg (control)', pr_cfg, 'ok'),
]
TAKE = "    /\\ cell' = None     \\* FaultCell::take\n"
failed = False
for name, cfg, expected in cases:
    this_spec = spec
    if cfg is None:
        this_spec = edit(spec, [(TAKE, '    /\\ UNCHANGED cell\n')], name)
        cfg = pr_cfg
    verdict, trace, out = tlc(jar, 'FaultIo', this_spec, cfg)
    if verdict == expected:
        print(f'{name}: {verdict}')
    else:
        print(f'{name}: expected {expected}, got {verdict}\n  {" -> ".join(trace)}\n{out}')
        failed = True
sys.exit(1 if failed else 0)
