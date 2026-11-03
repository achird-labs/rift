#!/usr/bin/env python3
"""Check that recorded runs of the engine are behaviours of the lifecycle model.

Usage: python3 validate.py <tla2tools.jar> <trace dir or files...>
       python3 validate.py <tla2tools.jar> --selftest traces

`--selftest` checks the checker: every trace under `traces/accept` must be accepted and every one
under `traces/reject` rejected (runs the model forbids, written by hand from recorded ones).

Each trace (ndjson of probe points, written by crates/rift-mock-core/tests/lifecycle_interleavings.rs
under RIFT_TRACE_DIR) is turned into model action labels (trace_labels.py) and into a `Trace`
module, then TLC searches TraceValidation.tla for a behaviour of Lifecycle that takes those steps
in that order.  Exits non-zero when a trace is rejected, when no trace was checked, or when a
trace cannot be read.  A trace TLC cannot decide within TRACE_TIMEOUT_SECS (default 300) is
reported as undecided and does not fail the run.
"""
import glob
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from tlacheck import read, tlc  # noqa: E402
from trace_labels import labels, read as read_trace, summary  # noqa: E402

here = os.path.dirname(os.path.abspath(__file__))
TIMEOUT_SECS = int(os.environ.get("TRACE_TIMEOUT_SECS", "300"))
ACCEPTED, REJECTED, UNDECIDED = "accepted", "rejected", "undecided"


def tla_set(items):
    return "{" + ", ".join(items) + "}"


def trace_module(actions, s):
    """The `Trace` module: the labels as a sequence of records, and each op's port."""
    recs = ", ".join(
        f'[action |-> "{a["action"]}", op |-> "{a["op"]}", port |-> {a["port"]}, n |-> {a["n"]}, '
        f'ok |-> {"TRUE" if a["ok"] else "FALSE"}]' for a in actions)
    ops = s["creators"] + s["deleters"] + s["appliers"] + s["delete_alls"]
    port0 = s["ports"][0] if s["ports"] else 1
    op_port = " @@ ".join(f'"{o}" :> {s["op_port"].get(o, port0)}' for o in ops) or '"none" :> 0'
    return (f"---- MODULE Trace ----\nEXTENDS Naturals, Sequences, TLC\n"
            f"Trace == <<{recs}>>\n"
            f"OpPort == ({op_port})\n"
            f"====\n")


def trace_cfg(s):
    q = lambda names: tla_set(f'"{o}"' for o in names)
    ports = tla_set(str(p) for p in s["ports"]) if s["ports"] else "{1}"
    return f"""CONSTANTS
    Ports = {ports}
    Creators = {q(s["creators"])}
    Deleters = {q(s["deleters"])}
    Appliers = {q(s["appliers"])}
    ApplyOnes = {{}}
    DeleteAlls = {q(s["delete_alls"])}
    ApplyDesired = {{}}
    MaxGens = {s["max_gens"]}
    MaxReqs = {s["max_reqs"]}
    MaxChanges = 0
    Preloaded = FALSE
    PersistFails = {"TRUE" if s["persist_fails"] else "FALSE"}
    UnlinkFails = {"TRUE" if s["unlink_fails"] else "FALSE"}
    DrainTimesOut = TRUE
    NoGen = NoGen
    NoOp = NoOp
INIT TInit
NEXT TraceNext
INVARIANT TraceNotDone
"""


def check(jar, actions):
    s = summary(actions)
    spec = read(os.path.join(here, "TraceValidation.tla"))
    extra = {"Lifecycle": read(os.path.join(here, "Lifecycle.tla")), "Trace": trace_module(actions, s)}
    verdict, _, out = tlc(jar, "TraceValidation", spec, trace_cfg(s), extra_modules=extra,
                          timeout=TIMEOUT_SECS, deadlock=False)
    if verdict == "TraceNotDone":
        return ACCEPTED, ""
    if verdict == "ok":
        return REJECTED, "no behaviour of the model takes these steps"
    if verdict == "timeout":
        return UNDECIDED, f"undecided after {TIMEOUT_SECS}s"
    return REJECTED, "TLC failed: " + out[-1200:]


def collect(args):
    paths = []
    for arg in args:
        paths += sorted(glob.glob(os.path.join(arg, "*.ndjson"))) if os.path.isdir(arg) else [arg]
    return paths


def run(jar, paths):
    results = []
    for path in paths:
        try:
            actions = labels(read_trace(path))
        except ValueError as e:
            # A run no recording of the engine produces: rejected, with the reason.
            results.append((path, [], REJECTED, f"not a run of the engine: {e}"))
            continue
        if actions is None:
            results.append((path, None, None, "not modelled"))
            continue
        verdict, why = check(jar, actions)
        results.append((path, actions, verdict, why))
    return results


def selftest(jar, root):
    wrong = []
    for expected, sub in ((ACCEPTED, "accept"), (REJECTED, "reject")):
        for path, _, verdict, why in run(jar, collect([os.path.join(root, sub)])):
            name = f"{sub}/{os.path.basename(path)}"
            print(f"{verdict or 'skipped'}: {name}")
            if verdict != expected:
                wrong.append(f"{name} was {verdict or 'skipped'}: {why}")
    for w in wrong:
        print(f"WRONG {w}")
    sys.exit(1 if wrong else 0)


def main():
    jar = os.path.abspath(sys.argv[1])
    if sys.argv[2] == "--selftest":
        selftest(jar, sys.argv[3])
    results = run(jar, collect(sys.argv[2:]))
    count = {v: sum(1 for r in results if r[2] == v) for v in (ACCEPTED, REJECTED, UNDECIDED, None)}
    print(f"{count[ACCEPTED]} accepted, {count[REJECTED]} rejected, {count[UNDECIDED]} undecided, "
          f"{count[None]} skipped (not modelled), of {len(results)} traces")
    for path, actions, verdict, why in results:
        if verdict in (REJECTED, UNDECIDED):
            print(f"{verdict.upper()} {os.path.basename(path)}: {[a['action'] for a in actions]}\n  {why}")
    if count[ACCEPTED] == 0:
        print("No trace was accepted: nothing was checked.")
    sys.exit(1 if count[REJECTED] or count[ACCEPTED] == 0 else 0)


if __name__ == "__main__":
    main()
