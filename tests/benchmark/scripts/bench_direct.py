#!/usr/bin/env python3
"""Direct-process Rift vs Mountebank benchmark (no Docker).

Each engine runs one at a time on a DISJOINT port range, so even if one fails
to shut down it can never be measured in place of the other. For each engine we
load an identical set of imposters, warm up, then drive a curated set of
scenarios with `oha`, capturing RPS and latency percentiles from oha's JSON.

Fairness / correctness safeguards:
  * engines run sequentially (never contend for CPU on a single machine),
  * disjoint ports per engine (rift offset 0, mb offset +100),
  * each engine launched in its own process group and killed by group + lsof,
  * the engine's ports are asserted free before launch and after teardown,
  * every scenario asserts the HTTP status distribution (a mis-served stub
    cannot silently inflate throughput).

Run everything (launches + stops both engines, writes the report):
    python3 bench_direct.py --run-all

Must be run OUTSIDE the CLI sandbox (via the sidecar) because `oha` needs
macOS keychain access to initialise TLS even for plain-HTTP targets.
"""
import argparse, csv, json, subprocess, sys, time, urllib.request, urllib.error, os, signal, shutil

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")

# Mirrors rift's journal cap (crates/rift-mock-core/src/imposter/journal.rs: MAX_RECORDED_REQUESTS).
# Under load the retained-entries count saturates here while numberOfRequests keeps climbing, so the
# recording_on assertion checks "journal filled AND cap respected" against this bound.
MAX_RECORDED_REQUESTS = 10_000

# ---- imposter config generation (identical JSON posted to both engines) ----

def api_stubs(resources=10, per=10):
    out = []
    for i in range(1, resources + 1):
        r = f"resource{i}"
        out.append({
            "predicates": [{"equals": {"method": "GET", "path": f"/api/v1/{r}"}}],
            "responses": [{"is": {"statusCode": 200,
                "headers": {"Content-Type": "application/json"},
                "body": json.dumps({"items": [{"id": 1}, {"id": 2}], "total": 2})}}],
        })
        for j in range(1, per + 1):
            out.append({
                "predicates": [{"equals": {"method": "GET", "path": f"/api/v1/{r}/{j}"}}],
                "responses": [{"is": {"statusCode": 200,
                    "headers": {"Content-Type": "application/json"},
                    "body": json.dumps({"id": j, "name": f"{r}_{j}"})}}],
            })
            out.append({
                "predicates": [{"equals": {"method": "PUT", "path": f"/api/v1/{r}/{j}"}}],
                "responses": [{"is": {"statusCode": 200, "body": json.dumps({"id": j, "updated": True})}}],
            })
            out.append({
                "predicates": [{"equals": {"method": "DELETE", "path": f"/api/v1/{r}/{j}"}}],
                "responses": [{"is": {"statusCode": 204}}],
            })
    return out

def regex_stubs(n=100):
    return [{
        "predicates": [{"matches": {"path": f"/regex/pattern{i}/[a-zA-Z0-9]+"}}],
        "responses": [{"is": {"statusCode": 200, "body": f"regex {i}"}}],
    } for i in range(1, n + 1)]

def complex_stubs(n=50):
    return [{
        "predicates": [{"and": [
            {"equals": {"method": "POST"}},
            {"startsWith": {"path": f"/complex/{i}/"}},
            {"or": [
                {"contains": {"headers": {"X-Request-Type": "json"}}},
                {"contains": {"headers": {"Content-Type": "application/json"}}},
            ]},
        ]}],
        "responses": [{"is": {"statusCode": 200,
            "headers": {"Content-Type": "application/json"},
            "body": json.dumps({"complex": i, "matched": True})}}],
    } for i in range(1, n + 1)]

def json_body_stubs(n=50):
    return [{
        "predicates": [{"equals": {"method": "POST", "path": f"/json/equals/{i}",
            "body": {"id": i, "type": "request"}}}],
        "responses": [{"is": {"statusCode": 200, "body": json.dumps({"matched": "equals", "id": i})}}],
    } for i in range(1, n + 1)]

def jsonpath_stubs(n=50):
    # Mountebank applies a jsonpath selector as a modifier on a SINGLE-operator predicate, so the
    # method/path match and the selected-value match must be separate predicates. Rift accepts the
    # same canonical form; a combined predicate silently fails to match on MB (falls through to the
    # empty no-match default), which would make the comparison bogus.
    return [{
        "predicates": [
            {"equals": {"method": "POST", "path": f"/jsonpath/{i}"}},
            {"equals": {"body": str(i)}, "jsonpath": {"selector": "$.user.id"}},
        ],
        "responses": [{"is": {"statusCode": 200, "body": json.dumps({"jsonpath_matched": True, "user_id": i})}}],
    } for i in range(1, n + 1)]

def xpath_stubs(n=50):
    # Canonical MB form (see jsonpath_stubs): method/path is one predicate, the xpath selector
    # modifies a second single-operator predicate. Rift matches the same shape.
    return [{
        "predicates": [
            {"equals": {"method": "POST", "path": f"/xpath/{i}"}},
            {"exists": {"body": True}, "xpath": {"selector": f"//item[@id='{i}']"}},
        ],
        "responses": [{"is": {"statusCode": 200,
            "headers": {"Content-Type": "application/xml"},
            "body": f"<response><id>{i}</id></response>"}}],
    } for i in range(1, n + 1)]

def template_stubs(n=50):
    return [{
        "predicates": [{"equals": {"path": f"/template/{i}"}}],
        "responses": [{"is": {"statusCode": 200,
            "headers": {"Content-Type": "application/json", "X-Request-Path": "${request.path}"},
            "body": '{"template": %d, "path": "${request.path}", "query": "${request.query}"}' % i}}],
    } for i in range(1, n + 1)]

def header_stubs(n=100):
    return [{
        "predicates": [{"equals": {"path": "/headers/route", "headers": {"X-Route-Id": f"route-{i}"}}}],
        "responses": [{"is": {"statusCode": 200, "body": json.dumps({"routed_to": i})}}],
    } for i in range(1, n + 1)]

def query_stubs(n=100):
    return [{
        "predicates": [{"equals": {"path": "/query/search", "query": {"page": str(i), "size": "10"}}}],
        "responses": [{"is": {"statusCode": 200, "body": json.dumps({"page": i})}}],
    } for i in range(1, n + 1)]

def simple_stubs():
    return [
        {"predicates": [{"equals": {"path": "/health"}}], "responses": [{"is": {"statusCode": 200, "body": "OK"}}]},
        {"predicates": [{"equals": {"path": "/ping"}}], "responses": [{"is": {"statusCode": 200, "body": "pong"}}]},
    ]

# base imposter ports (an engine offset is added to each)
IMPOSTERS = [
    (4549, "Simple", simple_stubs()),
    (4545, "API", api_stubs()),
    (4546, "Regex", regex_stubs()),
    (4547, "Complex", complex_stubs()),
    (4550, "JSONBody", json_body_stubs()),
    (4551, "JSONPath", jsonpath_stubs()),
    (4552, "XPath", xpath_stubs()),
    (4553, "Template", template_stubs()),
    (4554, "Header", header_stubs()),
    (4555, "Query", query_stubs()),
]

# scenarios: (name, base_port, method, path, body, headers)
SCENARIOS = [
    ("simple_health",     4549, "GET",  "/health", None, {}),
    ("api_first",         4545, "GET",  "/api/v1/resource1", None, {}),
    ("api_middle",        4545, "GET",  "/api/v1/resource5/5", None, {}),
    ("api_last",          4545, "GET",  "/api/v1/resource10/10", None, {}),
    ("no_match",          4545, "GET",  "/nonexistent", None, {}),
    ("regex_last",        4546, "GET",  "/regex/pattern100/test", None, {}),
    ("complex_predicate", 4547, "POST", "/complex/25/test", '{"name":"test"}', {"Content-Type": "application/json"}),
    ("json_body_equals",  4550, "POST", "/json/equals/25", '{"id":25,"type":"request"}', {"Content-Type": "application/json"}),
    ("jsonpath",          4551, "POST", "/jsonpath/25", '{"user":{"id":25,"name":"x"}}', {"Content-Type": "application/json"}),
    ("xpath",             4552, "POST", "/xpath/25", '<root><item id="25">x</item></root>', {"Content-Type": "application/xml"}),
    ("template",          4553, "GET",  "/template/25?foo=bar&baz=qux", None, {}),
    ("header_last",       4554, "GET",  "/headers/route", None, {"X-Route-Id": "route-100"}),
    ("query_last",        4555, "GET",  "/query/search?page=100&size=10", None, {}),
]

# both engines return an empty 200 as the default no-match response, so a 2xx status alone does
# NOT prove the intended stub matched — a mis-matching request falls through to that empty default
# and would inflate throughput. Each scenario therefore declares a substring its MATCHED body must
# contain (engine-agnostic: chosen to prove the match without asserting engine-specific rendering
# like template substitution). `no_match` is the control: its body MUST be empty.
EXPECT_BODY = {
    "simple_health": "OK",
    "api_first": '"total": 2',
    "api_middle": '"name": "resource5_5"',
    "api_last": '"name": "resource10_10"',
    "no_match": None,
    "regex_last": "regex 100",
    "complex_predicate": '"complex": 25',
    "json_body_equals": '"matched": "equals"',
    "jsonpath": '"jsonpath_matched": true',
    "xpath": "<id>25</id>",
    "template": '"template": 25',
    "header_last": '"routed_to": 100',
    "query_last": '"page": 100',
    # recording_on hits the same api_middle path on a recordRequests imposter, so it matches
    # the same stub and returns the same body marker.
    "recording_on": '"name": "resource5_5"',
}

# recording_on (issue #702): the same stub set as api_middle, but on an imposter with
# `recordRequests: true` so the journal write path is exercised under load. It is ADDITIVE —
# never part of the MB-comparison SCENARIOS above — and only runs in the Rift-only sweep.
RECORDING_PORT = 4556
RECORDING_SCENARIO = ("recording_on", RECORDING_PORT, "GET", "/api/v1/resource5/5", None, {})

def recording_imposter_config(offset):
    return {"port": RECORDING_PORT + offset, "protocol": "http", "name": "Recording",
            "recordRequests": True, "stubs": api_stubs()}

# ---- admin API helpers ----

def post_json(url, obj):
    data = json.dumps(obj).encode()
    req = urllib.request.Request(url, data=data, method="POST",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.status, r.read()

def delete(url):
    req = urllib.request.Request(url, method="DELETE")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code

def port_up(port, timeout=1):
    try:
        urllib.request.urlopen(f"http://localhost:{port}/", timeout=timeout)
        return True
    except urllib.error.HTTPError:
        return True   # answered (any status) => something is listening
    except Exception:
        return False

def wait_ready(admin_port, tries=120):
    for _ in range(tries):
        if port_up(admin_port):
            return True
        time.sleep(0.5)
    return False

def load_imposters(admin, offset, recording=False):
    delete(admin + "/imposters")
    configs = [{"port": port + offset, "protocol": "http", "name": name, "stubs": stubs}
               for port, name, stubs in IMPOSTERS]
    if recording:
        configs.append(recording_imposter_config(offset))
    for cfg in configs:
        status, body = post_json(admin + "/imposters", cfg)
        if status != 201:
            raise SystemExit(f"  ! create imposter {cfg['port']} ({cfg['name']}) failed: HTTP {status}: {body[:200]}")
    print(f"  loaded {len(configs)} imposters "
          f"({sum(len(c['stubs']) for c in configs)} stubs) at offset +{offset}")

# ---- oha runner ----

def parse_conn_list(s):
    """Parse a --sweep-connections value like "1,10,50,200" into a list of positive ints."""
    vals = [int(x.strip()) for x in s.split(",") if x.strip()]
    if not vals or any(v <= 0 for v in vals):
        raise ValueError(f"invalid connection list {s!r}: need one or more positive integers")
    return vals

def mode_label(rate):
    """CSV `mode` column: closed-loop, or open-loop tagged with the fixed arrival rate."""
    return "closed" if rate is None else f"open@{rate}"

VALID_ENGINES = ("rift", "mb")

def resolve_run_mode(engines_str, sweep_arg, open_loop, connections):
    """Resolve (engines, conn_list, rate, recording) from CLI inputs, rejecting unknown engines.
    Sweep and open-loop are Rift-only: they force engines=[rift] and enable the recording_on
    scenario. The default run keeps the requested engines and runs closed-loop at one connection
    count. Returning this as a pure tuple keeps the dispatch testable."""
    engines = [e.strip() for e in engines_str.split(",") if e.strip()]
    if not engines or any(e not in VALID_ENGINES for e in engines):
        raise ValueError(f"invalid --engines {engines_str!r}: choose from {','.join(VALID_ENGINES)}")
    sweep, open_loop_on = sweep_arg is not None, open_loop is not None
    if sweep or open_loop_on:
        engines = ["rift"]
    conn_list = parse_conn_list(sweep_arg) if sweep else [connections]
    recording = engines == ["rift"] and (sweep or open_loop_on)
    return engines, conn_list, open_loop, recording

def build_oha_cmd(url, method, body, headers, duration, conns, rate=None):
    """Build the oha argv. `rate` (requests/sec) switches oha to open-loop (`-q`), a fixed
    arrival rate that exposes coordinated-omission tail latency the closed-loop run hides."""
    cmd = ["oha", "-z", duration, "-c", str(conns), "--no-tui",
           "--output-format", "json", "-m", method]
    if rate is not None:
        cmd += ["-q", str(rate)]
    for k, v in headers.items():
        cmd += ["-H", f"{k}: {v}"]
    if body is not None:
        cmd += ["-d", body]
    cmd.append(url)
    return cmd

def run_oha(url, method, body, headers, duration, conns, rate=None):
    cmd = build_oha_cmd(url, method, body, headers, duration, conns, rate)
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=int(duration.rstrip("s")) + 30)
    if out.returncode != 0:
        raise RuntimeError(f"oha failed: {out.stderr[:300]}")
    return json.loads(out.stdout)

def verify_body(engine, name, method, url, body, headers):
    """Send one real request and prove the intended stub served it (not the empty no-match default).
    Aborts the run on a fall-through, so a mis-matching config can never be measured as fast."""
    data = body.encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            text = r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", "replace")
    marker = EXPECT_BODY[name]
    if marker is None:
        if text != "":
            raise SystemExit(f"{engine}/{name}: expected the empty no-match default, got {len(text)}B — aborting")
    elif marker not in text:
        raise SystemExit(
            f"{engine}/{name}: stub did not match (marker {marker!r} absent from {len(text)}B body) — "
            f"the request fell through to the no-match default; measuring this would be bogus. Aborting")

def metric(j):
    s = j["summary"]
    lat = j.get("latencyPercentiles", {})
    def ms(key):
        v = lat.get(key)
        return round(v * 1000, 3) if v is not None else None
    codes = j.get("statusCodeDistribution", {})
    return {
        "rps": round(s["requestsPerSec"], 1),
        # oha reports the 99.9th percentile under the "p99.9" key; capture it for tail-latency
        # comparisons (closed-loop hides the tail, open-loop exposes it).
        "p50_ms": ms("p50"), "p90_ms": ms("p90"), "p99_ms": ms("p99"), "p999_ms": ms("p99.9"),
        "avg_ms": round(s["average"] * 1000, 3),
        "codes": codes,
    }

# ---- results CSV (extended with `connections` + `mode` + `p999_ms`, issue #702) ----

CSV_HEADER = "scenario,connections,mode,rps,p50_ms,p90_ms,p99_ms,p999_ms,avg_ms"

def _csv_num(v):
    # A percentile can be legitimately absent (oha omits a p99.9 when a point has too few samples,
    # e.g. c=1); record it as an empty cell, never the literal string "None".
    return "" if v is None else v

def csv_row(name, conns, mode, m):
    cells = [name, conns, mode, m["rps"], _csv_num(m["p50_ms"]), _csv_num(m["p90_ms"]),
             _csv_num(m["p99_ms"]), _csv_num(m["p999_ms"]), m["avg_ms"]]
    return ",".join(str(c) for c in cells)

def write_rift_csv(path, rows):
    with open(path, "w") as f:
        f.write(CSV_HEADER + "\n")
        for name, conns, mode, m in rows:
            f.write(csv_row(name, conns, mode, m) + "\n")

def load_rift_csv(fh):
    """Read the extended results CSV into a list of dict rows (keyed by header name), so callers
    are insensitive to the added columns' positions."""
    return list(csv.DictReader(fh))

def journal_ok(number_of_requests, recorded_len, cap=MAX_RECORDED_REQUESTS):
    """The recording_on journal-depth invariant: the imposter counted requests AND retained a
    non-empty, cap-bounded window of them (the cap being respected under a flood is the point)."""
    return number_of_requests > 0 and 0 < recorded_len <= cap

def assert_journal_filled(engine, admin, offset):
    """After the recording_on run, prove the journal write path actually recorded and honoured the
    cap — otherwise a broken recorder would measure as fast while silently recording nothing."""
    port = RECORDING_PORT + offset
    with urllib.request.urlopen(f"{admin}/imposters/{port}", timeout=15) as r:
        detail = json.loads(r.read())
    n = detail.get("numberOfRequests", 0)
    recorded = len(detail.get("requests", []))
    if not journal_ok(n, recorded):
        raise SystemExit(
            f"{engine}/recording_on: journal assertion failed "
            f"(numberOfRequests={n}, recorded={recorded}, cap={MAX_RECORDED_REQUESTS}) — aborting")
    print(f"  journal ok: numberOfRequests={n}, recorded={recorded} (cap {MAX_RECORDED_REQUESTS})")

def bench(engine, admin_port, offset, duration, warmup, conn_list, rate=None, recording=False):
    os.makedirs(RESULTS_DIR, exist_ok=True)
    admin = f"http://localhost:{admin_port}"
    if not wait_ready(admin_port):
        raise SystemExit(f"{engine}: admin API not ready on {admin_port}")
    print(f"[{engine}] admin ready on {admin_port}; loading imposters")
    load_imposters(admin, offset, recording=recording)
    time.sleep(1)
    mode = mode_label(rate)
    scenarios = SCENARIOS + ([RECORDING_SCENARIO] if recording else [])
    rows = []
    for conns in conn_list:
        for name, base_port, method, path, body, headers in scenarios:
            url = f"http://localhost:{base_port + offset}{path}"
            verify_body(engine, name, method, url, body, headers)   # prove the stub matched (not fall-through)
            run_oha(url, method, body, headers, warmup, conns)      # warmup (discarded)
            m = metric(run_oha(url, method, body, headers, duration, conns, rate))
            total = sum(m["codes"].values())
            good = all(c.startswith("2") for c in m["codes"])
            status = "ok" if good and total > 0 else f"BAD codes={m['codes']}"
            print(f"  {name:20s} c={conns:<4d} {m['rps']:>10.1f} rps  "
                  f"p50={m['p50_ms']}ms p99={m['p99_ms']}ms p999={m['p999_ms']}ms  {status}")
            if not (good and total > 0):
                raise SystemExit(f"{engine}/{name}: unexpected status distribution {m['codes']} — aborting")
            if name == RECORDING_SCENARIO[0]:
                assert_journal_filled(engine, admin, offset)
            rows.append((name, conns, mode, m))
    csv_path = os.path.join(RESULTS_DIR, f"direct_{engine}.csv")
    write_rift_csv(csv_path, rows)
    print(f"[{engine}] wrote {csv_path}")

# ---- engine orchestration ----

def engine_ports(offset):
    return ([admin_port_for(offset)] + [p + offset for p, _, _ in IMPOSTERS]
            + [RECORDING_PORT + offset] + ([9090] if offset == 0 else []))

def admin_port_for(offset):
    return 2525 + offset

def free_ports(ports):
    """Force-free ports by killing whatever listens on them (lsof + SIGKILL)."""
    for p in ports:
        try:
            pids = subprocess.run(["lsof", "-ti", f"tcp:{p}"], capture_output=True, text=True).stdout.split()
        except Exception:
            pids = []
        for pid in pids:
            try:
                os.kill(int(pid), signal.SIGKILL)
                print(f"  freed port {p} (killed pid {pid})")
            except Exception:
                pass

def stop(proc, ports):
    if proc is not None:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except Exception:
            pass
        try:
            proc.wait(timeout=5)
        except Exception:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except Exception:
                pass
    # belt-and-suspenders: ensure the ports are actually free
    free_ports(ports)
    for _ in range(40):
        if not any(port_up(p) for p in ports):
            return
        time.sleep(0.25)
    raise SystemExit(f"ports still occupied after stop: {ports}")

def launch(cmd, logpath):
    lf = open(logpath, "w")
    return subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT, start_new_session=True)

def run_all(duration, warmup, conn_list, rift_bin, mb_bin, engines, rate=None, recording=False):
    os.makedirs(RESULTS_DIR, exist_ok=True)
    node = shutil.which("node") or "node"
    full_plan = [
        ("rift", 0,   [rift_bin, "--port", str(admin_port_for(0)), "--allow-injection", "--loglevel", "warn"]),
        ("mb",   100, [node, mb_bin, "start", "--port", str(admin_port_for(100)), "--allowInjection", "--loglevel", "warn"]),
    ]
    plan = [p for p in full_plan if p[0] in engines]
    for engine, offset, cmd in plan:
        ports = engine_ports(offset)
        free_ports(ports)                       # clean slate
        if any(port_up(p) for p in ports):
            raise SystemExit(f"{engine}: ports not free before launch: {ports}")
        print(f"[{engine}] launching: {' '.join(cmd)}")
        proc = launch(cmd, os.path.join(RESULTS_DIR, f"{engine}-engine.log"))
        try:
            # recording_on only applies to Rift's journal write path; MB never records here.
            bench(engine, admin_port_for(offset), offset, duration, warmup, conn_list,
                  rate=rate, recording=recording and engine == "rift")
        finally:
            stop(proc, ports)
    rift_ver = subprocess.run([rift_bin, "--version"], capture_output=True, text=True).stdout.strip() or "local"
    if "rift" in engines and "mb" in engines:
        mb_ver = subprocess.run([node, mb_bin, "--version"], capture_output=True, text=True).stdout.strip() or "2.9.1"
        report(rift_ver, mb_ver, duration, conn_list[0])
    elif engines == ["rift"]:
        rift_only_report(rift_ver, duration, conn_list, rate, recording)
    else:
        # engines == ["mb"]: no comparison possible (needs Rift too) and no Rift-only report to write.
        print(f"[report] engines={engines}: benched only Mountebank; no report written "
              f"(the comparison needs both rift and mb)")

def rift_only_report(rift_ver, duration, conn_list, rate, recording):
    """Rift-only sweep / open-loop report: scenario × (connections, mode) matrices of RPS and p999.
    The MB-comparison report is deliberately untouched so its historical single-point numbers stay
    comparable; this is the extra artefact the Turbo round consumes."""
    path = os.path.join(RESULTS_DIR, "direct_rift.csv")
    with open(path) as f:
        rows = load_rift_csv(f)
    cols = list(dict.fromkeys((int(r["connections"]), r["mode"]) for r in rows))
    cell = {(r["scenario"], (int(r["connections"]), r["mode"])): r for r in rows}
    scen_order = [s[0] for s in SCENARIOS] + ([RECORDING_SCENARIO[0]] if recording else [])
    def colhdr(k):
        c, mode = k
        return f"c={c}" if mode == "closed" else f"c={c} {mode}"
    def matrix(f, title, field, fmt):
        f.write(f"\n## {title}\n\n")
        f.write("| Scenario | " + " | ".join(colhdr(k) for k in cols) + " |\n")
        f.write("|---" + "|--:" * len(cols) + "|\n")
        for name in scen_order:
            cells = [fmt(cell[(name, k)][field]) if (name, k) in cell else "—" for k in cols]
            mark = " **(recording)**" if name == RECORDING_SCENARIO[0] else ""
            f.write(f"| {name}{mark} | " + " | ".join(cells) + " |\n")
    lat = lambda v: "n/a" if v in (None, "") else v   # an absent percentile renders n/a, not blank
    out = os.path.join(RESULTS_DIR, "DIRECT_RIFT_SWEEP_REPORT.md")
    with open(out, "w") as f:
        f.write("# Rift — Concurrency Sweep / Open-Loop (issue #702)\n\n")
        f.write(f"- **Date:** {time.strftime('%Y-%m-%d %H:%M:%S')}\n- **Rift:** {rift_ver}\n")
        f.write(f"- **Load generator:** oha, {duration} per point (after warmup)\n")
        f.write(f"- **Connections:** {','.join(str(c) for c in conn_list)}\n")
        if rate is not None:
            f.write(f"- **Open-loop rate:** {rate} req/s (oha -q, fixed arrival rate)\n")
        if recording:
            f.write(f"- **recording_on:** imposter with recordRequests=true; journal-depth asserted "
                    f"per point (filled, cap {MAX_RECORDED_REQUESTS} respected)\n")
        matrix(f, "Throughput (requests/sec, higher is better)", "rps",
               lambda v: f"{float(v):,.0f}")
        matrix(f, "Latency p50 (ms, lower is better)", "p50_ms", lat)
        matrix(f, "Latency p99 (ms, lower is better)", "p99_ms", lat)
        matrix(f, "Tail latency p999 (ms, lower is better)", "p999_ms", lat)
    print(f"wrote {out}")

def report(rift_ver, mb_ver, duration, conns):
    def load(engine):
        path = os.path.join(RESULTS_DIR, f"direct_{engine}.csv")
        with open(path) as f:
            # The comparison is single-point closed-loop; ignore any sweep/open-loop rows a prior
            # run may have left in the CSV so a standalone --report can't blend the wrong point in.
            d = {r["scenario"]: {"rps": float(r["rps"]), "p50": r["p50_ms"], "p99": r["p99_ms"]}
                 for r in load_rift_csv(f)
                 if r["mode"] == "closed" and int(r["connections"]) == conns}
        return d
    rift, mb = load("rift"), load("mb")
    out = os.path.join(RESULTS_DIR, "DIRECT_BENCHMARK_REPORT.md")
    order = [s[0] for s in SCENARIOS]
    with open(out, "w") as f:
        f.write("# Rift vs Mountebank — Direct-Process Benchmark\n\n")
        f.write(f"- **Date:** {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
        f.write(f"- **Rift:** {rift_ver}\n- **Mountebank:** {mb_ver}\n")
        f.write(f"- **Load generator:** oha, {conns} keep-alive connections, {duration} per scenario (after warmup)\n")
        f.write("- **Method:** native processes (no Docker); engines run one at a time on disjoint "
                "port ranges (no CPU contention, no cross-talk); identical imposter configs; response "
                "status distribution asserted per scenario.\n\n")
        f.write("## Throughput (requests/sec, higher is better)\n\n")
        f.write("| Scenario | Mountebank | Rift | Speedup |\n|---|--:|--:|--:|\n")
        for name in order:
            mr, rr = mb[name]["rps"], rift[name]["rps"]
            sp = f"{rr/mr:.1f}x" if mr else "n/a"
            f.write(f"| {name} | {mr:,.0f} | {rr:,.0f} | **{sp}** |\n")
        f.write("\n## Latency p99 (ms, lower is better)\n\n| Scenario | Mountebank | Rift |\n|---|--:|--:|\n")
        for name in order:
            f.write(f"| {name} | {mb[name]['p99']} | {rift[name]['p99']} |\n")
    print(f"wrote {out}")

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", default="20s")
    ap.add_argument("--warmup", default="3s")
    ap.add_argument("--connections", type=int, default=50)
    ap.add_argument("--sweep-connections",
                    help="comma-separated connection counts, e.g. 1,10,50,200 — run each scenario at "
                         "each count (Rift-only; forces --engines rift)")
    ap.add_argument("--open-loop", type=int, metavar="RATE",
                    help="open-loop mode: fixed arrival rate (oha -q RATE) instead of closed-loop "
                         "(Rift-only; forces --engines rift)")
    ap.add_argument("--engines", default="rift,mb",
                    help="comma-separated engines to run (default: rift,mb). Sweep/open-loop are "
                         "Rift-only and override this to rift.")
    ap.add_argument("--run-all", action="store_true")
    ap.add_argument("--report", action="store_true")
    ap.add_argument("--rift-bin", default=os.path.join(os.path.dirname(__file__), "..", "..", "..", "target", "release", "rift-http-proxy"))
    ap.add_argument("--mb-bin", default=os.path.expanduser("~/bench-mb/node_modules/mountebank/bin/mb"))
    ap.add_argument("--rift-version", default="local")
    ap.add_argument("--mb-version", default="2.9.1")
    a = ap.parse_args()
    if a.run_all:
        try:
            engines, conn_list, rate, recording = resolve_run_mode(
                a.engines, a.sweep_connections, a.open_loop, a.connections)
        except ValueError as e:
            raise SystemExit(str(e))
        requested = [e.strip() for e in a.engines.split(",") if e.strip()]
        if engines != requested:
            print("note: sweep/open-loop is Rift-only; running --engines rift")
        run_all(a.duration, a.warmup, conn_list, a.rift_bin, a.mb_bin, engines,
                rate=rate, recording=recording)
    elif a.report:
        report(a.rift_version, a.mb_version, a.duration, a.connections)
    else:
        raise SystemExit("use --run-all (or --report)")
