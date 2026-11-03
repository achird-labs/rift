"""Turn a recorded run (the lifecycle probe points a test recorded) into the model actions it took.

Input: ndjson lines {"seq": n, "point": "<Point name>", "port": p, "generation": g, ...payload},
possibly out of order (several tasks record).  Output: a list of actions, each
{"action": label, "op": "<model op>", "port": p, "n": k, "ok": bool}, or None when the run uses
something the model does not cover.

Generations: the engine numbers imposters across all ports; the model numbers them per port, in
creation order, so `n` is the generation's rank among the CreateBound points of its port.

Ops: the model names every operation.  A CreateBound starts a creator (c1, c2, ...) unless an
apply_config or apply_one is reconciling its desired ports, which this labeller does not attribute
(a run with such a create is not covered).  A DeleteLocked starts a delete sequence; it belongs to
the delete_all or apply_config sweep that is open and still owes deletes, else to a new deleter
(d1, d2, ...).  Two sweeps open at once, or a deleter racing an open sweep, cannot be told apart
from the points alone, so a run with two sweeps is not covered either.  `persist_lock` serialises
every delete sequence from DeleteLocked to its DeleteRemoved/DeleteNotFound, which is what lets the
points of one sequence be paired without a task id.

The mapping (formal/lifecycle/README.md, "Trace validation"):
  CreateBound                   -> CreateBindOk
  CreateFloorRead               -> CreateReadFloor
  CreateAccepting               -> CreateSpawnAccept
  CreateClaimed / ClaimLost     -> CreateTryClaim, ok = won
  CreatePersisted / RolledBack  -> CreatePersistWrite, ok = persisted; RolledBack also -> CreateRollback
  DeleteLocked                  -> DeleteLock
  DeleteLookedUp / NotFound     -> DeleteLookup, ok = found
  DeleteUnlinked / UnlinkFailed -> DeleteUnlink, ok = unlinked
  DeleteFloorRecorded           -> DeleteRecordFloor
  DeleteRemoved / NotFound      -> DeleteRemoveIfSame, ok = removed
  DeleteCancelled               -> DeleteCancel
  DeleteDrained                 -> DeleteDrain
  DeleteReturned                -> DeleteClearShared
  ApplySwept / DeleteAllSwept   -> ApplySnapshot, n = ports swept
  Served                        -> ReqCompute of that generation (free ReqStart/ReqWrite around it)
"""
import json


def read(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def labels(records):
    records = sorted(records, key=lambda r: r["seq"])
    # Per-port generation ranks, from the engine's generation numbers.
    rank = {}
    per_port = {}
    for r in records:
        if r["point"] == "CreateBound":
            per_port.setdefault(r["port"], []).append(r["generation"])
    for port, gens in per_port.items():
        for k, g in enumerate(gens, 1):
            rank[g] = (port, k)
    gen_port = {g: p for g, (p, _) in rank.items()}

    out = []
    creator_of = {}      # engine generation -> creator op
    creators = deleters = sweeps = 0
    open_sweep = None    # (op, deletes still owed)
    in_delete = None     # (op, port, generation or None) for the sequence holding persist_lock
    draining = {}        # engine generation -> op whose delete of it is past the lock

    def gen_fields(r):
        g = r["generation"]
        if g not in rank:
            raise ValueError(f"generation {g} has no CreateBound point")
        port, n = rank[g]
        return port, n

    for r in records:
        p = r["point"]
        if p == "CreateBound":
            if open_sweep is not None and open_sweep[1] == 0:
                return None  # an apply's entry create: not attributed
            creators += 1
            o = f"c{creators}"
            creator_of[r["generation"]] = o
            port, n = gen_fields(r)
            out.append({"action": "CreateBindOk", "op": o, "port": port, "n": n, "ok": True})
        elif p in ("CreateFloorRead", "CreateAccepting"):
            port, n = gen_fields(r)
            out.append({"action": {"CreateFloorRead": "CreateReadFloor", "CreateAccepting": "CreateSpawnAccept"}[p],
                        "op": creator_of[r["generation"]], "port": port, "n": n, "ok": True})
        elif p in ("CreateClaimed", "CreateClaimLost"):
            port, n = gen_fields(r)
            out.append({"action": "CreateTryClaim", "op": creator_of[r["generation"]], "port": port, "n": n,
                        "ok": p == "CreateClaimed"})
        elif p in ("CreatePersisted", "CreateRolledBack"):
            port, n = gen_fields(r)
            o = creator_of[r["generation"]]
            out.append({"action": "CreatePersistWrite", "op": o, "port": port, "n": n, "ok": p == "CreatePersisted"})
            if p == "CreateRolledBack":
                out.append({"action": "CreateRollback", "op": o, "port": port, "n": n, "ok": True})
        elif p in ("ApplySwept", "DeleteAllSwept"):
            if open_sweep is not None:
                return None  # two sweeps at once: not attributed
            sweeps += 1
            o = ("a" if p == "ApplySwept" else "x") + str(sweeps)
            open_sweep = [o, r["swept"]]
            out.append({"action": "ApplySnapshot", "op": o, "port": 0, "n": r["swept"], "ok": True})
        elif p == "DeleteLocked":
            if in_delete is not None:
                raise ValueError("a DeleteLocked while another delete holds persist_lock")
            if open_sweep is not None and open_sweep[1] > 0:
                o = open_sweep[0]
                open_sweep[1] -= 1
            else:
                deleters += 1
                o = f"d{deleters}"
            in_delete = [o, r["port"], None]
            out.append({"action": "DeleteLock", "op": o, "port": r["port"], "n": 0, "ok": True})
        elif p == "DeleteLookedUp":
            if in_delete is None:
                return None  # a replace's lookup (no lock): not modelled yet
            port, n = gen_fields(r)
            in_delete[2] = r["generation"]
            out.append({"action": "DeleteLookup", "op": in_delete[0], "port": port, "n": n, "ok": True})
        elif p == "DeleteNotFound":
            if in_delete is None:
                return None
            o, port, g = in_delete
            action = "DeleteLookup" if g is None else "DeleteRemoveIfSame"
            out.append({"action": action, "op": o, "port": port, "n": 0, "ok": False})
            in_delete = None
        elif p in ("DeleteUnlinked", "DeleteUnlinkFailed"):
            port, n = gen_fields(r)
            out.append({"action": "DeleteUnlink", "op": in_delete[0], "port": port, "n": n,
                        "ok": p == "DeleteUnlinked"})
            if p == "DeleteUnlinkFailed":
                in_delete = None
        elif p == "DeleteFloorRecorded":
            port, n = gen_fields(r)
            out.append({"action": "DeleteRecordFloor", "op": in_delete[0], "port": port, "n": n, "ok": True})
        elif p == "DeleteRemoved":
            port, n = gen_fields(r)
            o = in_delete[0]
            draining[r["generation"]] = o
            in_delete = None
            out.append({"action": "DeleteRemoveIfSame", "op": o, "port": port, "n": n, "ok": True})
        elif p in ("DeleteCancelled", "DeleteDrained", "DeleteReturned"):
            port, n = gen_fields(r)
            if r["generation"] not in draining:
                raise ValueError(f"{p} for generation {r['generation']} before its DeleteRemoved")
            o = draining[r["generation"]]
            action = {"DeleteCancelled": "DeleteCancel", "DeleteDrained": "DeleteDrain",
                      "DeleteReturned": "DeleteClearShared"}[p]
            out.append({"action": action, "op": o, "port": port, "n": n, "ok": True})
        elif p == "Served":
            port, n = gen_fields(r)
            out.append({"action": "Served", "op": "req", "port": port, "n": n, "ok": True})
        else:
            return None  # a point the model does not cover
    return out


def summary(actions):
    """The constants a trace needs: ops by kind, ports, the per-port generation bound, the
    per-generation request bound, and which failures it recorded."""
    ops = {k: sorted({a["op"] for a in actions if a["op"].startswith(k)}, key=lambda s: int(s[1:]))
           for k in "cdax"}
    ports = sorted({a["port"] for a in actions if a["port"]})
    max_gens = max([a["n"] for a in actions if a["action"] == "CreateBindOk"] + [1])
    served = {}
    for a in actions:
        if a["action"] == "Served":
            served[(a["port"], a["n"])] = served.get((a["port"], a["n"]), 0) + 1
    return {
        "creators": ops["c"], "deleters": ops["d"], "appliers": ops["a"], "delete_alls": ops["x"],
        "ports": ports, "max_gens": max_gens, "max_reqs": max(list(served.values()) + [1]),
        "persist_fails": any(a["action"] == "CreateRollback" for a in actions),
        "unlink_fails": any(a["action"] == "DeleteUnlink" and not a["ok"] for a in actions),
        "op_port": {a["op"]: a["port"] for a in actions if a["op"] != "req" and a["port"]},
    }
