#!/usr/bin/env python3
"""Admin-plane create/read benchmark: Rift vs Mountebank.

Where `bench_direct.py` measures request *serving* throughput, this measures the cost of the
admin control plane: **creating** an imposter with many stubs and **reading** it back. That is the
path Rift's stub-overlap analysis lives on (issue #423) — the analysis is a Rift extension that
Mountebank does not perform, so this is where the two engines' admin behaviour differs most.

For each engine and each (predicate shape, stub count) it launches a FRESH engine process (so the
RSS delta is isolated), POSTs one imposter, then repeatedly GETs it, recording:

  * create latency        — POST /imposters with N stubs
  * GET latency (x5)       — GET /imposters/:port (Rift now serves cached warnings; see #423)
  * process RSS delta      — engine memory growth from the create
  * response body size     — the create/GET payload
  * warnings               — entries under `_rift.warnings` (Mountebank: always 0)

Two shapes are exercised: `identical/overlap` (all stubs share one predicate — the O(n²)-prone
case Rift #423 fixed) and `distinct` (the cheap control). Engines run one at a time on disjoint
admin ports, mirroring `bench_direct.py`.

Repetition (issue #1157). One sample of a ~8 ms create spans roughly 3–8 ms on one binary in one
session, so a single sample cannot carry a claim. Every point is measured `--rep` times and
reported as a median with its spread. A discarded warm-up round runs first, and the arms are
interleaved within each round with their order rotated per round, so no arm always absorbs the
cold start or a drifting host. `--rift-bin` is repeatable as `label=path`, which makes a multi-arm
A/B one command; `--engines rift` leaves Mountebank out of it.

Every run writes under its own `--tag` (default: a timestamp), so a second run never overwrites the
first: `results/ADMIN_BENCHMARK_REPORT_<tag>.md`, the raw samples in
`results/admin_samples_<tag>.json`, and one engine log per measurement in `results/admin-<tag>/`.

Usage:
  python3 bench_admin.py --run-all --rep 5 \
      --rift-bin ../../../target/release/rift-http-proxy \
      --mb-bin ~/bench-mb/node_modules/mountebank/bin/mb

  # A/B of two Rift builds, Mountebank left out
  python3 bench_admin.py --run-all --rep 9 --engines rift \
      --rift-bin old=/tmp/rift-old --rift-bin new=../../../target/release/rift-http-proxy
"""
import argparse, json, os, shutil, signal, subprocess, sys, time, urllib.request, urllib.error

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bench_direct import _median, _spread_pct  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS_DIR = os.path.join(HERE, "..", "results")

GETS = 5
SIZES = [100, 1000]
DEFAULT_REP = 5
ENGINES = ("rift", "mb")
RIFT_ADMIN = 2525
MB_ADMIN = 2625          # disjoint from Rift, matching bench_direct.py's +100 offset
DP_PORT = 4900           # data-plane port for the imposter under test (reused across sequential runs)


# ── stub shapes ─────────────────────────────────────────────────────────────
def identical_stubs(n):
    """All stubs share one predicate — every pair overlaps (the #423 pathology)."""
    return [{"predicates": [{"equals": {"path": "/data"}}],
             "responses": [{"is": {"statusCode": 200, "body": "x"}}]} for _ in range(n)]


def distinct_stubs(n):
    """Distinct predicates — the cheap control."""
    return [{"predicates": [{"equals": {"path": f"/p{i}"}}],
             "responses": [{"is": {"statusCode": 200, "body": "x"}}]} for i in range(n)]


SHAPES = [("identical/overlap", identical_stubs), ("distinct", distinct_stubs)]


# ── process / http helpers (self-contained; conventions mirror bench_direct.py) ──
def port_up(port, timeout=1):
    try:
        urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=timeout)
        return True
    except urllib.error.HTTPError:
        return True  # any HTTP response means the admin plane is listening
    except Exception:
        return False


def wait_ready(port, tries=120):
    for _ in range(tries):
        if port_up(port):
            return True
        time.sleep(0.25)
    return False


def free_ports(ports):
    for p in ports:
        try:
            pids = subprocess.run(["lsof", "-ti", f"tcp:{p}"], capture_output=True, text=True).stdout.split()
        except Exception:
            pids = []
        for pid in pids:
            try:
                os.kill(int(pid), signal.SIGKILL)
            except Exception:
                pass


def launch(cmd, logpath):
    lf = open(logpath, "w")
    return subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT, start_new_session=True)


def stop(proc, ports):
    if proc is not None:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            proc.wait(timeout=5)
        except Exception:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except Exception:
                pass
    free_ports(ports)
    for _ in range(40):
        if not any(port_up(p) for p in ports):
            return
        time.sleep(0.25)


def rss_mb(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True)
    try:
        return int(out.stdout.strip()) / 1024.0
    except ValueError:
        return float("nan")


def post(url, obj):
    body = json.dumps(obj).encode()
    req = urllib.request.Request(url, data=body, method="POST",
                                 headers={"Content-Type": "application/json"})
    t = time.time()
    resp = urllib.request.urlopen(req, timeout=180)
    raw = resp.read()
    return resp.status, raw, (time.time() - t) * 1000


def get_ms(url):
    t = time.time()
    urllib.request.urlopen(url, timeout=180).read()
    return (time.time() - t) * 1000


def warning_count(raw):
    try:
        return len(json.loads(raw).get("_rift", {}).get("warnings", []))
    except Exception:
        return 0


# ── one measurement: fresh engine, create, read, teardown ───────────────────
def measure(cmd, admin_port, extra_ports, shape_build, n, logpath):
    ports = [admin_port, DP_PORT] + extra_ports
    free_ports(ports)
    proc = launch(cmd, logpath)
    try:
        if not wait_ready(admin_port):
            raise SystemExit(f"engine admin not ready on {admin_port}")
        time.sleep(0.3)
        base = rss_mb(proc.pid)
        status, body, create_ms = post(f"http://127.0.0.1:{admin_port}/imposters",
                                        {"port": DP_PORT, "protocol": "http", "stubs": shape_build(n)})
        if status not in (200, 201):
            raise SystemExit(f"create failed: HTTP {status}: {body[:200]!r}")
        peak = rss_mb(proc.pid)
        gets = sorted(get_ms(f"http://127.0.0.1:{admin_port}/imposters/{DP_PORT}") for _ in range(GETS))
        return {
            "create_ms": create_ms,
            "rss_delta_mb": peak - base,
            "body_mb": len(body) / 1024.0 / 1024.0,
            "warnings": warning_count(body),
            "get_min_ms": gets[0],
            "get_med_ms": gets[len(gets) // 2],
        }
    finally:
        stop(proc, ports)


# ── arms, schedule, aggregation (pure; pinned by test_bench_admin.py) ───────
def parse_rift_bins(values):
    """`--rift-bin` values → [(label, path)]. A bare path is labelled `rift`; `label=path` names an
    arm. Labels must be unique and must not be `mb`, or two arms' samples would merge."""
    values = values or [os.path.join(HERE, "..", "..", "..", "target", "release", "rift-http-proxy")]
    arms = []
    for v in values:
        label, sep, path = v.partition("=")
        if not sep:
            label, path = "rift", v
        if not label or not path:
            raise ValueError(f"--rift-bin {v!r}: expected PATH or LABEL=PATH")
        arms.append((label, os.path.abspath(os.path.expanduser(path))))
    labels = [label for label, _ in arms]
    if len(set(labels)) != len(labels) or "mb" in labels:
        raise ValueError(f"--rift-bin labels must be unique and not 'mb': {labels}")
    return arms


def parse_engines(value):
    engines = [e.strip() for e in value.split(",") if e.strip()]
    unknown = [e for e in engines if e not in ENGINES]
    if not engines or unknown:
        raise ValueError(f"--engines {value!r}: expected a comma list of {', '.join(ENGINES)}")
    return engines


def schedule(rounds, points, arms):
    """Every (round, point, arm) measurement, in the order it runs.

    Round 0 is the warm-up. Within a round each point runs every arm back to back, so the arms of a
    point are measured close together in time; the arm order rotates by one per round, so no arm is
    always first after a gap."""
    order = []
    for rnd in range(rounds):
        k = rnd % len(arms)
        rotated = arms[k:] + arms[:k]
        for point in points:
            for arm in rotated:
                order.append((rnd, point, arm))
    return order


def aggregate(samples):
    """Samples → per-(arm, shape, n) statistics. `samples` holds only measured (non-warm-up) rounds:
    a list of dicts with `arm`, `shape`, `n` and the metrics `measure()` returns."""
    grouped = {}
    for smp in samples:
        grouped.setdefault((smp["arm"], smp["shape"], smp["n"]), []).append(smp)
    out = {}
    for key, rows in grouped.items():
        def col(name):
            return [r[name] for r in rows]
        creates = col("create_ms")
        out[key] = {
            "reps": len(rows),
            "create_med_ms": _median(creates),
            "create_min_ms": min(creates),
            "create_max_ms": max(creates),
            "create_spread_pct": _spread_pct(creates),
            "get_med_ms": _median(col("get_med_ms")),
            "rss_delta_med_mb": _median(col("rss_delta_mb")),
            "rss_delta_spread_pct": _spread_pct(col("rss_delta_mb")),
            "body_mb": _median(col("body_mb")),
            "warnings": _median(col("warnings")),
        }
    return out


def output_paths(tag):
    """Where one run's artefacts go. Everything is keyed on the tag, so runs never overwrite."""
    return {
        "report": os.path.join(RESULTS_DIR, f"ADMIN_BENCHMARK_REPORT_{tag}.md"),
        "samples": os.path.join(RESULTS_DIR, f"admin_samples_{tag}.json"),
        "logs": os.path.join(RESULTS_DIR, f"admin-{tag}"),
    }


def log_name(arm, shape, n, rnd):
    kind = "warmup" if rnd == 0 else f"rep{rnd}"
    return f"{arm}-{shape.replace('/', '-')}-{n}-{kind}.log"


# ── run ─────────────────────────────────────────────────────────────────────
def engine_arms(rift_bins, engines, mb_bin, node):
    """(label, admin port, command, extra ports) per arm, Rift builds first."""
    arms = []
    if "rift" in engines:
        for label, path in rift_bins:
            arms.append((label, RIFT_ADMIN,
                         [path, "--port", str(RIFT_ADMIN), "--loglevel", "warn"], [9090]))
    if "mb" in engines:
        arms.append(("mb", MB_ADMIN,
                     [node, mb_bin, "start", "--port", str(MB_ADMIN), "--loglevel", "warn"], []))
    return arms


def run_all(rift_bins, mb_bin, engines, rep, tag):
    paths = output_paths(tag)
    os.makedirs(paths["logs"], exist_ok=True)
    node = shutil.which("node") or "node"
    arms = engine_arms(rift_bins, engines, mb_bin, node)
    points = [(shape, build, n) for shape, build in SHAPES for n in SIZES]
    samples = []
    for rnd, (shape, build, n), (label, admin, cmd, extra) in schedule(rep + 1, points, arms):
        kind = "warm-up" if rnd == 0 else f"rep {rnd}/{rep}"
        print(f"[{label}] {shape}  N={n}  ({kind}) ...", flush=True)
        m = measure(cmd, admin, extra, build, n,
                    os.path.join(paths["logs"], log_name(label, shape, n, rnd)))
        print(f"    create {m['create_ms']:.1f} ms | GET~{m['get_med_ms']:.1f} ms | "
              f"RSS +{m['rss_delta_mb']:.1f} MB | body {m['body_mb']:.3f} MB | "
              f"warnings {m['warnings']}")
        if rnd > 0:
            samples.append({"arm": label, "shape": shape, "n": n, "round": rnd, **m})
    with open(paths["samples"], "w") as f:
        json.dump(samples, f, indent=1)
    versions = {label: version_of(cmd[:1] if label != "mb" else cmd[:2]) for label, _, cmd, _ in arms}
    write_report(aggregate(samples), [label for label, *_ in arms], versions, rep, paths["report"])


def version_of(cmd):
    try:
        return subprocess.run(cmd + ["--version"], capture_output=True, text=True).stdout.strip() or "?"
    except Exception:
        return "?"


def render_report(stats, arm_labels, versions, rep, date):
    lines = ["# Admin create/read benchmark", "",
             f"- **Date:** {date}"]
    for label in arm_labels:
        lines.append(f"- **{label}:** {versions.get(label, '?')}")
    lines += [
        f"- **Method:** fresh engine process per (arm, shape, N); {rep} measured rounds after one "
        "discarded warm-up round, arms interleaved within each round and their order rotated per "
        "round. Create = `POST /imposters` with N stubs; GET = median of 5 "
        "`GET /imposters/:port`; RSS via `ps` right after the create.",
        "- Figures are medians over the rounds; `spread` is peak-to-peak as a % of the mean. "
        "`body` is the create response size — equal across Rift builds, a cross-check that the "
        "arms did the same work.",
        "- Stub-overlap analysis is a Rift extension (issue #423); Mountebank does none, so its "
        "`warnings` are always 0.",
        "",
    ]
    for shape, _ in SHAPES:
        lines += [f"## Shape: {shape}", "",
                  "| N | Arm | Reps | Create med (ms) | Create min–max (ms) | Spread | "
                  "GET med (ms) | RSS Δ med (MB) | RSS spread | Body (MB) | Warnings |",
                  "|--:|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|"]
        for n in SIZES:
            for label in arm_labels:
                st = stats.get((label, shape, n))
                if st is None:
                    continue
                lines.append(
                    f"| {n} | {label} | {st['reps']} | {st['create_med_ms']:.2f} | "
                    f"{st['create_min_ms']:.2f}–{st['create_max_ms']:.2f} | "
                    f"{st['create_spread_pct']:.0f}% | {st['get_med_ms']:.2f} | "
                    f"{st['rss_delta_med_mb']:.2f} | {st['rss_delta_spread_pct']:.0f}% | "
                    f"{st['body_mb']:.3f} | {st['warnings']:g} |")
        lines.append("")
    return "\n".join(lines)


def write_report(stats, arm_labels, versions, rep, out):
    with open(out, "w") as f:
        f.write(render_report(stats, arm_labels, versions, rep, time.strftime("%Y-%m-%d %H:%M:%S")))
    print(f"\nwrote {out}")
    return out


def positive_int(value):
    n = int(value)
    if n < 1:
        raise argparse.ArgumentTypeError(f"must be at least 1, got {n}")
    return n


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--run-all", action="store_true")
    ap.add_argument("--rift-bin", action="append",
                    help="PATH or LABEL=PATH; repeat for an A/B (default: "
                         "target/release/rift-http-proxy, labelled rift)")
    ap.add_argument("--mb-bin", default=os.path.expanduser("~/bench-mb/node_modules/mountebank/bin/mb"))
    ap.add_argument("--engines", default="rift,mb", help="comma list of rift, mb (default: both)")
    ap.add_argument("--rep", type=positive_int, default=DEFAULT_REP,
                    help=f"measured rounds per point, after one discarded warm-up (default {DEFAULT_REP})")
    ap.add_argument("--tag", default=time.strftime("%Y%m%d-%H%M%S"),
                    help="names this run's artefacts (default: a timestamp)")
    a = ap.parse_args()
    if a.run_all:
        try:
            rift_bins, engines = parse_rift_bins(a.rift_bin), parse_engines(a.engines)
        except ValueError as e:
            ap.error(str(e))
        run_all(rift_bins, os.path.expanduser(a.mb_bin), engines, a.rep, a.tag)
    else:
        ap.print_help()
