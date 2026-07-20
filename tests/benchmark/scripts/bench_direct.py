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
import argparse, csv, glob, json, re, subprocess, sys, threading, time, urllib.request, urllib.error, os, signal, shutil

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

DEFAULT_JSON_BODY_STUBS = 50

def set_json_body_stub_count(n):
    """Rebuild the JSONBody imposter with `n` field-equals-on-body stubs (#779).

    Stub count is the axis this dimension is *about*: the quamina prefilter replaces an O(N) scan
    of structural comparisons, so its win is a function of how many such stubs compete for one
    request. Benching only the default 50 would measure one arbitrary point on that curve.

    The `json_body_equals` SCENARIO must be retargeted with the imposter: it addresses one specific
    stub by index, so scaling the stubs alone leaves it pointing at a stub that no longer exists —
    the request falls through to the no-match default and the run aborts on the body assertion.
    The target stays the *middle* stub (`n // 2`), which is what the scenario has always used
    (25 of 50), so the default configuration is byte-identical to before."""
    global IMPOSTERS, SCENARIOS
    IMPOSTERS = [(port, name, json_body_stubs(n) if name == "JSONBody" else stubs)
                 for port, name, stubs in IMPOSTERS]
    target = max(1, n // 2)
    SCENARIOS = [
        (name, port, method, f"/json/equals/{target}",
         json.dumps({"id": target, "type": "request"}, separators=(",", ":")), headers)
        if name == "json_body_equals" else (name, port, method, path, body, headers)
        for name, port, method, path, body, headers in SCENARIOS
    ]

def json_body_stubs(n=DEFAULT_JSON_BODY_STUBS):
    return [{
        "predicates": [{"equals": {"method": "POST", "path": f"/json/equals/{i}",
            "body": {"id": i, "type": "request"}}}],
        "responses": [{"is": {"statusCode": 200, "body": json.dumps({"matched": "equals", "id": i})}}],
    } for i in range(1, n + 1)]

DEFAULT_BODY_FIELD_STUBS = 200

def body_field_stubs(n=DEFAULT_BODY_FIELD_STUBS):
    """N stubs on ONE shared path+method, discriminated ONLY by a body field value.

    This is the workload the quamina body-field dimension (#767) exists for, and the suite had
    nothing like it. `json_body_stubs` gives every stub a unique path, so the path dimension prunes
    the candidate set to one before the body is consulted — the automaton then re-derives what the
    path index already knew, and the run measures pure overhead rather than the dimension's benefit
    (observed: -8% at every N in run 29738479074, flat with N, which is the signature of measuring
    overhead alone).

    Sharing the path across all N is what forces the body field to be the discriminator, so the
    `O(N)` structural scan the dimension replaces is actually on the critical path."""
    return [{
        "predicates": [{"equals": {"method": "POST", "path": "/orders/submit",
            "body": {"orderId": f"ord-{i}", "channel": "web"}}}],
        "responses": [{"is": {"statusCode": 200,
            "body": json.dumps({"body_field_matched": True, "order": i})}}],
    } for i in range(1, n + 1)]

def set_body_field_stub_count(n):
    """Scale the BodyField imposter and retarget its scenario together (#784's lesson)."""
    global IMPOSTERS, DIMENSION_SCENARIOS
    IMPOSTERS = [(port, name, body_field_stubs(n) if name == "BodyField" else stubs)
                 for port, name, stubs in IMPOSTERS]
    target = max(1, n // 2)
    DIMENSION_SCENARIOS = [
        (name, port, method, path,
         json.dumps({"orderId": f"ord-{target}", "channel": "web"}, separators=(",", ":")), headers)
        if name == "body_field_scale" else (name, port, method, path, body, headers)
        for name, port, method, path, body, headers in DIMENSION_SCENARIOS
    ]

def deepequals_body_stubs(n=50):
    """`deepEquals`-on-body stubs — the dimension #740 indexes by structural body hash.

    Distinct from `json_body_stubs`, which uses `equals`: the two take different index paths
    (structural hash vs the quamina field automaton), and until now the suite exercised only the
    latter, so #740's `O(stubs x body)` -> `O(body)` change was measured by nothing."""
    return [{
        "predicates": [{"deepEquals": {"method": "POST", "path": f"/deep/equals/{i}",
            "body": {"order": {"id": i, "items": [{"sku": f"sku-{i}", "qty": i}]},
                     "meta": {"source": "bench"}}}}],
        "responses": [{"is": {"statusCode": 200,
            "body": json.dumps({"matched": "deepEquals", "order": i})}}],
    } for i in range(1, n + 1)]

def literal_prefix_stubs(n=100):
    """`startsWith` / `contains` path stubs — the anchored/unanchored Aho-Corasick literal
    dimension (#732). The suite had exactly one `startsWith` and two `contains` across ~860 stubs,
    so a single-pass literal scan over many anchors was effectively untested."""
    out = []
    for i in range(1, n + 1):
        out.append({
            "predicates": [{"startsWith": {"path": f"/literal/prefix{i}/"}}],
            "responses": [{"is": {"statusCode": 200, "body": json.dumps({"literal": i, "kind": "prefix"})}}],
        })
        out.append({
            "predicates": [{"contains": {"path": f"/needle{i}/"}}],
            "responses": [{"is": {"statusCode": 200, "body": json.dumps({"literal": i, "kind": "contains"})}}],
        })
    return out

def method_mix_stubs(n=50):
    """Same path, different verbs — the method candidate dimension (#729).

    Every pre-existing scenario is GET or POST, so a request whose method is the *only* thing
    separating it from n-1 other stubs never happened. Sharing one path across the verbs is what
    forces the method dimension to do the pruning."""
    verbs = ("PUT", "DELETE", "PATCH", "OPTIONS")
    return [{
        "predicates": [{"equals": {"method": verb, "path": f"/method/{i}"}}],
        "responses": [{"is": {"statusCode": 200,
            "body": json.dumps({"method_matched": verb, "id": i})}}],
    } for i in range(1, n + 1) for verb in verbs]

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
    (4560, "DeepEquals", deepequals_body_stubs()),
    (4557, "Literal", literal_prefix_stubs()),
    (4558, "MethodMix", method_mix_stubs()),
    (4559, "BodyField", body_field_stubs()),
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
# Scenarios covering optimizations the MB-comparison set does not reach. Kept SEPARATE because
# that set is a stability contract — it must stay byte-comparable with previously published
# Mountebank numbers (see `DefaultRunUnchanged` in the tests) — so these are additive, exactly like
# `recording_on`. They run in Rift-only sweeps.
DIMENSION_SCENARIOS = [
    # Issue #740: deepEquals-on-body, indexed by structural body hash. Zero coverage before this.
    ("deepequals_body",   4560, "POST", "/deep/equals/25",
     '{"order":{"id":25,"items":[{"sku":"sku-25","qty":25}]},"meta":{"source":"bench"}}',
     {"Content-Type": "application/json"}),
    # Issue #732: single-pass literal dimension, anchored (startsWith) and unanchored (contains).
    ("literal_prefix",    4557, "GET",  "/literal/prefix100/deep/path", None, {}),
    ("literal_contains",  4557, "GET",  "/x/needle100/y", None, {}),
    # Issue #729: method is the ONLY discriminator across these stubs.
    ("method_mix",        4558, "DELETE", "/method/25", None, {}),
    # Issue #767: one shared path, body field is the ONLY discriminator — the workload the
    # quamina dimension indexes. Scale it with --body-field-stubs.
    ("body_field_scale",  4559, "POST", "/orders/submit",
     '{"orderId":"ord-100","channel":"web"}', {"Content-Type": "application/json"}),
]

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
    "deepequals_body": '"matched": "deepEquals"',
    "literal_prefix": '"kind": "prefix"',
    "literal_contains": '"kind": "contains"',
    "method_mix": '"method_matched": "DELETE"',
    "body_field_scale": '"body_field_matched": true',
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
    # Per-imposter progress. Without it, a crash during loading tells you only that it happened
    # somewhere across 14 creations: run 29751530395 died here with SIGKILL, no OOM evidence and
    # memory flat, and the summary print at the end meant there was nothing to localise it with.
    for i, cfg in enumerate(configs, 1):
        t0 = time.time()
        status, body = post_json(admin + "/imposters", cfg)
        if status != 201:
            raise SystemExit(f"  ! create imposter {cfg['port']} ({cfg['name']}) failed: HTTP {status}: {body[:200]}")
        print(f"    [{i}/{len(configs)}] {cfg['name']} port={cfg['port']} "
              f"stubs={len(cfg['stubs'])} in {time.time() - t0:.2f}s")
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

def build_oha_cmd(url, method, body, headers, duration, conns, rate=None, prefix=()):
    """Build the oha argv. `rate` (requests/sec) switches oha to open-loop (`-q`), a fixed
    arrival rate that exposes coordinated-omission tail latency the closed-loop run hides.
    `prefix` is an optional launcher (e.g. `taskset -c ...`) that confines the generator to
    its own CPUs when the engine is core-pinned (#746 core-count axis)."""
    cmd = list(prefix) + ["oha", "-z", duration, "-c", str(conns), "--no-tui",
                          "--output-format", "json", "-m", method]
    if rate is not None:
        cmd += ["-q", str(rate)]
    for k, v in headers.items():
        cmd += ["-H", f"{k}: {v}"]
    if body is not None:
        cmd += ["-d", body]
    cmd.append(url)
    return cmd

def run_oha(url, method, body, headers, duration, conns, rate=None, prefix=()):
    cmd = build_oha_cmd(url, method, body, headers, duration, conns, rate, prefix)
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

# ---- allocator bake-off (issue #717): build-arg selection + RSS sampling ----

REPO_ROOT = os.path.join(os.path.dirname(__file__), "..", "..", "..")
ALLOCATORS = ("mimalloc", "jemalloc", "system")
DEFAULT_RIFT_BIN = os.path.join(REPO_ROOT, "target", "release", "rift-http-proxy")

def allocator_build_args(name):
    """Cargo flags that swap ONLY the global allocator, keeping redis-backend+javascript on in
    every variant so the three builds are functionally identical apart from the allocator (#717)."""
    if name == "mimalloc":
        return []   # default features already include mimalloc
    if name == "jemalloc":
        return ["--no-default-features", "--features", "redis-backend,javascript,jemalloc"]
    if name == "system":
        return ["--no-default-features", "--features", "redis-backend,javascript"]
    raise ValueError(f"unknown allocator {name!r}: choose from {','.join(ALLOCATORS)}")

# ---- quamina body-field dimension A/B (issue #779): same discipline as --allocator ----
#
# The dimension's go/no-go numbers (#767) were taken against rift-mock-core, the one crate where the
# feature was enabled — it was compiled out of the binary and the C-ABI until #777. So the shipped
# win has never been measured. This axis builds the two variants and benches them identically.

QUAMINA_VARIANTS = ("on", "off")
QUAMINA_MARKER = "Matching dimensions: "
QUAMINA_PROBE_PORT = 3527

def quamina_build_args(variant):
    """Cargo flags that swap ONLY the body-field dimension, keeping redis-backend + javascript +
    mimalloc on in both variants so the builds are functionally identical apart from it."""
    if variant == "on":
        return []   # default features include quamina-matching (#777)
    if variant == "off":
        return ["--no-default-features", "--features", "redis-backend,javascript,mimalloc"]
    raise ValueError(f"unknown quamina variant {variant!r}: choose from {','.join(QUAMINA_VARIANTS)}")

def quamina_bin_path(variant):
    """Per-variant binary path, so the two builds coexist instead of clobbering target/release."""
    return os.path.join(REPO_ROOT, "target", f"quamina-{variant}", "release", "rift-http-proxy")

def build_quamina_binary(variant):
    print(f"building rift with quamina body-field dimension={variant}")
    cmd = ["cargo", "build", "--release", "-p", "rift-http-proxy",
           "--target-dir", os.path.join("target", f"quamina-{variant}")] + quamina_build_args(variant)
    out = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True)
    if out.returncode != 0:
        tail = "\n".join((out.stdout + out.stderr).splitlines()[-40:])
        raise SystemExit(f"build failed for quamina={variant}:\n{tail}")
    print(f"  built target/quamina-{variant}/release/rift-http-proxy")

def extract_quamina_marker(text):
    """Pull the self-reported dimension state out of an engine log, or None if absent."""
    for line in text.splitlines():
        if QUAMINA_MARKER in line:
            return line.split(QUAMINA_MARKER, 1)[1].strip()
    return None

def quamina_marker_matches(reported, variant):
    """The binary reports `body-field(quamina)=on|off`; the label must match the request."""
    return reported == f"body-field(quamina)={variant}"

def verify_quamina_marker(rift_bin, variant):
    """Probe-launch and require the binary's OWN self-report to match the requested variant.

    This is the #777 lesson made mechanical: that issue shipped a dimension enabled in the library
    and compiled out of everything users run, and no artefact said so. Benching a build whose
    dimension state is assumed rather than read would repeat it — with the added twist that the
    two variants are supposed to produce identical matching RESULTS, so nothing else in the run
    would look wrong."""
    os.makedirs(RESULTS_DIR, exist_ok=True)
    free_ports([QUAMINA_PROBE_PORT])
    logpath = os.path.join(RESULTS_DIR, "quamina-probe.log")
    proc = launch([rift_bin, "--port", str(QUAMINA_PROBE_PORT), "--loglevel", "info"], logpath)
    try:
        reported = None
        for _ in range(40):
            time.sleep(0.25)
            with open(logpath) as f:
                reported = extract_quamina_marker(f.read())
            if reported is not None:
                break
        if reported is None or not quamina_marker_matches(reported, variant):
            raise SystemExit(
                f"quamina probe: binary reports {reported!r}, requested variant {variant!r} "
                f"({rift_bin}) — aborting rather than benching a mislabeled build. Both variants "
                f"match identically by design, so a mislabel would not show up anywhere else.")
        print(f"  quamina probe ok: {reported}")
    finally:
        stop(proc, [QUAMINA_PROBE_PORT])

def binary_size_mb(path):
    """Size of the benched binary in MB — the dimension pulls a dependency into the server binary
    and, for embedders, into the cdylib they link, so size is part of its cost (#779)."""
    try:
        return round(os.path.getsize(path) / (1024 * 1024), 2)
    except OSError:
        return None

def allocator_bin_path(name):
    """Per-allocator binary path; a separate --target-dir per allocator so three builds can
    coexist on disk instead of clobbering each other's `target/release` (#717)."""
    return os.path.join(REPO_ROOT, "target", f"alloc-{name}", "release", "rift-http-proxy")

def resolve_rift_bin(rift_bin_arg, allocator, quamina=None):
    """Resolve (path, needs_build). An explicit --rift-bin is always trusted verbatim (no build,
    even if --allocator is also set). No allocator selected: fall back to the default release
    path, unchanged from pre-#717 behaviour. Allocator selected with no --rift-bin: build into
    the per-allocator target dir."""
    if rift_bin_arg is not None:
        return rift_bin_arg, False
    if quamina is not None:
        return quamina_bin_path(quamina), True
    if allocator is None:
        return DEFAULT_RIFT_BIN, False
    return allocator_bin_path(allocator), True

def build_allocator_binary(name):
    """Build the bake-off binary for one allocator into target/alloc-<name>/ (#717)."""
    print(f"building rift with allocator={name}")
    cmd = ["cargo", "build", "--release", "-p", "rift-http-proxy",
           "--target-dir", os.path.join("target", f"alloc-{name}")] + allocator_build_args(name)
    out = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True)
    if out.returncode != 0:
        tail = "\n".join((out.stdout + out.stderr).splitlines()[-40:])
        raise SystemExit(f"build failed for allocator={name}:\n{tail}")
    print(f"  built target/alloc-{name}/release/rift-http-proxy")

def parse_rss_kb(text):
    """Parse `ps -o rss=` output (KB); absent/garbage reads as None, never 0 (#717)."""
    try:
        return int(text.strip())
    except ValueError:
        return None

class RssSampler(threading.Thread):
    """Samples the engine's RSS (ps -o rss=, KB) once a second while a bench runs, so each
    scenario row can report peak/end memory (#717). Daemon: never blocks harness exit."""

    def __init__(self, pid):
        super().__init__(daemon=True)
        self.pid = pid
        self.samples = []
        # NOT named `_stop`: threading.Thread has an internal `_stop()` method that
        # `join()` calls on Python <=3.12 — shadowing it with an Event makes every
        # join() raise `TypeError: 'Event' object is not callable` (found the hard
        # way on ubuntu runners; macOS's Python 3.13 join() never calls it).
        self._stop_event = threading.Event()

    def run(self):
        while not self._stop_event.wait(1.0):
            try:
                out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.pid)],
                                      capture_output=True, text=True)
                kb = parse_rss_kb(out.stdout)
            except Exception:
                kb = None   # engine may be restarting/gone — a missing sample is not an error
            if kb is not None:
                self.samples.append((time.time(), kb))

    def stop(self):
        self._stop_event.set()
        self.join(timeout=2)

    def window(self, since_ts):
        """(peak_mb, end_mb) over samples with ts >= since_ts; (None, None) if none in window."""
        kbs = [kb for ts, kb in self.samples if ts >= since_ts]
        if not kbs:
            return None, None
        return round(max(kbs) / 1024, 1), round(kbs[-1] / 1024, 1)

# ---- results CSV (extended with `connections` + `mode` + `p999_ms`, issue #702; +rss, #717) ----

CSV_HEADER = "scenario,connections,mode,rps,p50_ms,p90_ms,p99_ms,p999_ms,avg_ms,rss_mb_peak,rss_mb_end"

def _csv_num(v):
    # A percentile can be legitimately absent (oha omits a p99.9 when a point has too few samples,
    # e.g. c=1); record it as an empty cell, never the literal string "None".
    return "" if v is None else v

def csv_row(name, conns, mode, m):
    cells = [name, conns, mode, m["rps"], _csv_num(m["p50_ms"]), _csv_num(m["p90_ms"]),
             _csv_num(m["p99_ms"]), _csv_num(m["p999_ms"]), m["avg_ms"],
             _csv_num(m.get("rss_mb_peak")), _csv_num(m.get("rss_mb_end"))]
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

def bench(engine, admin_port, offset, duration, warmup, conn_list, rate=None, recording=False,
          pid=None, csv_suffix="", oha_prefix=()):
    os.makedirs(RESULTS_DIR, exist_ok=True)
    admin = f"http://localhost:{admin_port}"
    if not wait_ready(admin_port):
        raise SystemExit(f"{engine}: admin API not ready on {admin_port}")
    print(f"[{engine}] admin ready on {admin_port}; loading imposters")
    load_imposters(admin, offset, recording=recording)
    time.sleep(1)
    mode = mode_label(rate)
    # Dimension scenarios ride with the Rift-only sweeps (same rule as recording_on).
    scenarios = SCENARIOS + (DIMENSION_SCENARIOS + [RECORDING_SCENARIO] if recording else [])
    rows = []
    sampler = RssSampler(pid) if pid is not None else None
    if sampler is not None:
        sampler.start()
    try:
        for conns in conn_list:
            for name, base_port, method, path, body, headers in scenarios:
                url = f"http://localhost:{base_port + offset}{path}"
                verify_body(engine, name, method, url, body, headers)   # prove the stub matched (not fall-through)
                run_oha(url, method, body, headers, warmup, conns,
                        prefix=oha_prefix)                             # warmup (discarded)
                t0 = time.time()
                m = metric(run_oha(url, method, body, headers, duration, conns, rate,
                                   prefix=oha_prefix))
                if sampler is not None:
                    m["rss_mb_peak"], m["rss_mb_end"] = sampler.window(t0)
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
    finally:
        if sampler is not None:
            sampler.stop()
    csv_path = write_results_csv(engine, csv_suffix, rows)
    print(f"[{engine}] wrote {csv_path}")

# ---- engine orchestration ----

def engine_ports(offset):
    return ([admin_port_for(offset)] + [p + offset for p, _, _ in IMPOSTERS]
            + [RECORDING_PORT + offset] + ([9090] if offset == 0 else []))

def admin_port_for(offset):
    return 2525 + offset

def free_ports(ports):
    """Force-free ports by killing whatever LISTENS on them (lsof + SIGKILL).

    `-sTCP:LISTEN` is load-bearing, not tidiness. A bare `lsof -ti tcp:PORT` matches every socket
    on that port including the *client* end, so the harness's own connections to the admin port
    made it a candidate for its own SIGKILL. That is not hypothetical: it masked a real error for
    four CI runs. `load_imposters` raised SystemExit("Port 4556 is already in use"), the `finally`
    in `run_all` called this function during cleanup, and the interpreter was killed before it
    could print the message — turning an actionable one-line error into a bare exit 137 with no
    output at all. The self-PID guard below is belt-and-braces for the same reason: nothing here
    should ever be able to kill the process doing the killing."""
    own = {os.getpid(), os.getpgrp()}
    for p in ports:
        try:
            pids = subprocess.run(["lsof", "-ti", f"tcp:{p}", "-sTCP:LISTEN"],
                                  capture_output=True, text=True).stdout.split()
        except Exception:
            pids = []
        for pid in pids:
            try:
                pid_i = int(pid)
            except ValueError:
                continue
            if pid_i in own:
                print(f"  refusing to kill self (pid {pid_i}) while freeing port {p}")
                continue
            try:
                os.kill(pid_i, signal.SIGKILL)
                print(f"  freed port {p} (killed pid {pid_i})")
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

# The binary self-reports its allocator at startup: `Global allocator: <name>` (#717). That line
# is info-level, and bench engines run at --loglevel warn (per #718, extra logging on a measured
# run is itself a perf distortion) — so verification is a separate one-second probe launch, not
# part of the measured run.
ALLOC_MARKER = "Global allocator: "
ALLOC_PROBE_PORT = 3525

def extract_allocator_marker(text):
    """Pull the self-reported allocator name out of an engine log, or None if absent."""
    for line in text.splitlines():
        if ALLOC_MARKER in line:
            return line.split(ALLOC_MARKER, 1)[1].strip()
    return None

def verify_allocator_marker(rift_bin, expected):
    """Probe-launch the binary (info level, scratch admin port) and require its self-reported
    allocator to equal the requested one. Without this, a stale/wrong --rift-bin — or a
    mis-featured build — would stamp a whole sweep with the wrong label and feed #717's
    pre-registered decision rule bogus data, silently."""
    os.makedirs(RESULTS_DIR, exist_ok=True)
    free_ports([ALLOC_PROBE_PORT])
    logpath = os.path.join(RESULTS_DIR, "allocator-probe.log")
    proc = launch([rift_bin, "--port", str(ALLOC_PROBE_PORT), "--loglevel", "info"], logpath)
    try:
        name = None
        for _ in range(40):
            time.sleep(0.25)
            with open(logpath) as f:
                name = extract_allocator_marker(f.read())
            if name is not None:
                break
        if name != expected:
            raise SystemExit(
                f"allocator probe: binary reports {name!r}, expected {expected!r} ({rift_bin}) "
                f"— aborting so the sweep is not mislabeled")
        print(f"  allocator probe ok: {name}")
    finally:
        stop(proc, [ALLOC_PROBE_PORT])

# ---- runtime topology pass-through (issue #746): same discipline as --allocator ----

RUNTIME_MODES = ("work-stealing", "per-core")
TOPOLOGY_MARKER = "Runtime topology: "
TOPOLOGY_PROBE_PORT = 3526

def runtime_launch_args(mode):
    """Engine flags for the requested topology; None = engine default (today's work-stealing)."""
    if mode is None:
        return []
    if mode in RUNTIME_MODES:
        return ["--runtime", mode]
    raise ValueError(f"unknown runtime mode {mode!r}: choose from {','.join(RUNTIME_MODES)}")

def extract_topology_marker(text):
    """Pull the self-reported EFFECTIVE topology out of an engine log, or None if absent."""
    for line in text.splitlines():
        if TOPOLOGY_MARKER in line:
            return line.split(TOPOLOGY_MARKER, 1)[1].strip()
    return None

def topology_matches(reported, requested, expected_workers=None):
    """`per-core` self-reports as `per-core xN`; work-stealing reports itself verbatim.

    Under the core-count axis `expected_workers` is the pinned CPU budget, and per-core's N must
    equal it — that is the one observable proving `taskset` actually reached the engine's worker
    sizing. It transitively covers work-stealing too: both topologies size from the same
    `available_parallelism()`, which work-stealing never reports."""
    if expected_workers is not None and requested == "per-core":
        return reported == f"per-core x{expected_workers}"
    return reported == requested or reported.startswith(requested + " ")

def verify_runtime_marker(rift_bin, mode, extra_args, prefix=(), expected_workers=None):
    """Probe-launch and require the binary's self-reported topology to match the request.
    This is what refuses a mislabeled sweep on macOS, where a requested per-core falls back
    to work-stealing with a warning (RFC-712 D5) — the fallback is correct engine behavior,
    but benching it under a per-core label would poison the #746 decision data."""
    os.makedirs(RESULTS_DIR, exist_ok=True)
    free_ports([TOPOLOGY_PROBE_PORT])
    logpath = os.path.join(RESULTS_DIR, "topology-probe.log")
    proc = launch(
        list(prefix)
        + [rift_bin, "--port", str(TOPOLOGY_PROBE_PORT), "--loglevel", "info"] + extra_args,
        logpath,
    )
    try:
        reported = None
        for _ in range(40):
            time.sleep(0.25)
            with open(logpath) as f:
                reported = extract_topology_marker(f.read())
            if reported is not None:
                break
        if reported is None or not topology_matches(reported, mode, expected_workers):
            want = mode if expected_workers is None else f"{mode} x{expected_workers}"
            raise SystemExit(
                f"topology probe: binary reports {reported!r}, requested {want!r} ({rift_bin}) "
                f"— aborting so the sweep is not mislabeled (on macOS per-core falls back to "
                f"work-stealing by design; run the per-core sweep on Linux. A worker-count "
                f"mismatch means the CPU pinning did not reach the engine's worker sizing, so "
                f"the core-count axis would be a fiction)")
        print(f"  topology probe ok: {reported}")
    finally:
        stop(proc, [TOPOLOGY_PROBE_PORT])

# ---- core-count axis (#746 / RFC-712): pin the engine to N CPUs, the generator to the rest ----
#
# RFC-712's thesis is about the *slope* of RPS vs cores, so the sweep has to vary cores, not just
# connections. Two hazards make a naive `--runtime per-core=N` insufficient:
#
#   1. Work-stealing must be sized to the same N, or the comparison is 16-thread-vs-N-thread.
#      Both modes derive their worker count from `available_parallelism()`, which on Linux honours
#      sched_getaffinity — so one `taskset` sizes *both* topologies identically and truthfully.
#   2. oha runs on the same box. Unpinned, it steals from the very cores under measurement, and on
#      an SMT host it can land on the sibling thread of an engine core — contention that reads as
#      a scaling ceiling. So the split is made on *physical-core* boundaries: the generator never
#      shares a core with the engine.

def parse_lscpu_topology(text):
    """Parse `lscpu -p=CPU,CORE` output into [(cpu_id, core_id), ...] in file order.
    Comment lines (`#`) carry the header and are skipped."""
    pairs = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        cpu, core = line.split(",")[:2]
        pairs.append((int(cpu), int(core)))
    if not pairs:
        raise ValueError("could not parse any CPU/core rows from lscpu output")
    return pairs

def plan_cpu_split(pairs, server_cpus):
    """Split the machine's CPUs into (engine_cpus, generator_cpus) so that no physical core is
    shared between the two — whole cores go to the engine, whole cores to the generator.

    `server_cpus` is the CPU budget the engine sees (what `available_parallelism()` returns, hence
    the worker count for both topologies). It must fall on a core boundary; SMT siblings are
    indivisible here on purpose, since handing the generator an engine core's sibling is exactly
    the contamination this split exists to prevent."""
    cores = {}
    for cpu, core in pairs:
        cores.setdefault(core, []).append(cpu)
    order = list(cores)                      # cores in first-seen order
    engine, taken = [], []
    for core in order:
        if len(engine) >= server_cpus:
            break
        engine += cores[core]
        taken.append(core)
    if len(engine) != server_cpus:
        boundaries = []
        acc = 0
        for core in order:
            acc += len(cores[core])
            boundaries.append(acc)
        raise ValueError(
            f"server core budget {server_cpus} does not fall on a physical-core boundary; "
            f"valid budgets on this host: {','.join(str(b) for b in boundaries)}")
    generator = [cpu for core in order if core not in taken for cpu in cores[core]]
    if not generator:
        raise ValueError(
            f"server core budget {server_cpus} leaves no CPUs for the load generator "
            f"(host has {len(pairs)}) — the generator must never share the engine's cores")
    return sorted(engine), sorted(generator)

def cpuset_arg(cpus):
    """Render a CPU list as a taskset -c argument."""
    return ",".join(str(c) for c in cpus)

def taskset_prefix(cpus):
    """Launcher prefix confining a process to `cpus`; empty when no pinning was requested."""
    return ["taskset", "-c", cpuset_arg(cpus)] if cpus else []

def resolve_cpu_split(server_cpus):
    """Read this host's topology and plan the engine/generator split, or (None, None) when the
    core-count axis is off. Linux-only: taskset and lscpu -p are the mechanism."""
    if server_cpus is None:
        return None, None
    if shutil.which("taskset") is None or shutil.which("lscpu") is None:
        raise SystemExit("--server-cores needs taskset and lscpu (Linux only) — "
                         "run the core-count axis on the Linux runner, per RFC-712 D5")
    out = subprocess.run(["lscpu", "-p=CPU,CORE"], capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(f"lscpu -p failed: {out.stderr[:300]}")
    try:
        return plan_cpu_split(parse_lscpu_topology(out.stdout), server_cpus)
    except ValueError as e:
        raise SystemExit(str(e))

def result_suffix(allocator, runtime_mode, server_cpus=None, rep=None,
                  quamina=None, stub_count=None, body_field_stubs=None):
    """Allocator, runtime, core-count and repetition dimensions compose into one artefact suffix so
    no combination overwrites another's CSV/report (#717, #746, #773).

    `rep` is what stops a repeated run from being self-destructive. It used to be a *caller*
    concept — the sweep workflow looped and copied the file afterwards — so every rep overwrote the
    unsuffixed name and the artefact bundle ended up with a canonical-looking file holding one
    unreplicated sample (#773). Threading it through here means a repped run never writes that
    file at all."""
    suffix = f"_{allocator}" if allocator else ""
    if runtime_mode:
        suffix += f"_{runtime_mode}"
    if server_cpus:
        suffix += f"_cores{server_cpus}"
    if quamina:
        suffix += f"_quamina{quamina}"
    if stub_count:
        suffix += f"_stubs{stub_count}"
    if body_field_stubs:
        suffix += f"_bfstubs{body_field_stubs}"
    # rep stays outermost: every other dimension names a *variant*, a rep names a sample OF one.
    if rep is not None:
        suffix += f"_rep{rep}"
    return suffix

# Numeric CSV columns an aggregate takes the median of. `rps` additionally carries a spread so a
# degraded rep is visible rather than silently folded into the middle value.
AGGREGATE_FIELDS = ("rps", "p50_ms", "p90_ms", "p99_ms", "p999_ms", "avg_ms",
                    "rss_mb_peak", "rss_mb_end")

def _median(values):
    """Median of a numeric list; the even case averages the middle two."""
    s = sorted(values)
    mid = len(s) // 2
    return s[mid] if len(s) % 2 else (s[mid - 1] + s[mid]) / 2

def _spread_pct(values):
    """Peak-to-peak spread as a percentage of the mean — the number that would have exposed the
    #746 degraded rep (~20% low on one of three runs) instead of it passing as a normal sample."""
    mean = sum(values) / len(values)
    return 0.0 if mean == 0 else (max(values) - min(values)) / mean * 100.0

def aggregate_reps(reps):
    """Collapse N repetitions into one median-per-point aggregate, keyed by
    (scenario, connections, mode).

    `reps` is a list of rep row-lists, each shaped like `load_rift_csv` output. Every point carries
    `reps` (how many samples backed it) and `rps_spread_pct`, because a median alone cannot tell a
    clean run from one where a rep was measured on a degraded machine — which is exactly how #746's
    headline number went wrong."""
    if not reps:
        raise ValueError("aggregate_reps needs at least one repetition")
    points = {}
    for rows in reps:
        for r in rows:
            key = (r["scenario"], int(r["connections"]), r["mode"])
            points.setdefault(key, []).append(r)
    out = {}
    for key, samples in points.items():
        cell = {"reps": len(samples)}
        for field in AGGREGATE_FIELDS:
            # A percentile can be legitimately absent (oha omits p99.9 when a point has too few
            # samples). An absent value stays absent rather than being counted as zero, which
            # would drag the median toward a number nothing measured.
            vals = [float(r[field]) for r in samples if r.get(field) not in (None, "")]
            cell[field] = _median(vals) if vals else ""
        rps_vals = [float(r["rps"]) for r in samples if r.get("rps") not in (None, "")]
        cell["rps_spread_pct"] = _spread_pct(rps_vals) if rps_vals else ""
        out[key] = cell
    return out

def results_csv_path(engine, csv_suffix):
    """The one place a run's results CSV name is built."""
    return os.path.join(RESULTS_DIR, f"direct_{engine}{csv_suffix}.csv")

def write_results_csv(engine, csv_suffix, rows):
    """Write a run's results through a single seam, so "which files does a run produce" is a
    testable property rather than a fact buried in `bench()` behind a live server (#773).

    An existing target is overwritten — reruns of the same rep are legitimate after a crash — but
    never silently: the whole point of #773 is that a results file must not change under you
    without saying so."""
    path = results_csv_path(engine, csv_suffix)
    if os.path.exists(path):
        print(f"  note: overwriting existing {os.path.basename(path)}")
    write_rift_csv(path, rows)
    return path

REP_FILE_RX = re.compile(r"_rep(\d+)\.csv$")

def find_rep_files(base_suffix):
    """Every `_repN.csv` written for one variant, in rep order.

    Matches on the `_rep<digits>.csv` tail so a *different* variant sharing a prefix, or a stale
    unsuffixed file from a pre-#773 run, is never swept into the aggregate."""
    pattern = os.path.join(RESULTS_DIR, f"direct_rift{base_suffix}_rep*.csv")
    matched = []
    for p in glob.glob(pattern):
        m = REP_FILE_RX.search(p)
        if m:
            matched.append((int(m.group(1)), p))
    return [p for _, p in sorted(matched)]

def aggregate_reps_to_report(base_suffix, rift_ver):
    """Read a variant's `_repN.csv` files, write the median-of-reps CSV + report, return the report
    path. This is the artefact the unsuffixed filename used to *imply* and never was (#773)."""
    paths = find_rep_files(base_suffix)
    if not paths:
        raise SystemExit(
            f"no repetition files matched direct_rift{base_suffix}_rep*.csv in {RESULTS_DIR} — "
            f"nothing to aggregate (run the sweep with --rep N first)")
    reps = []
    for p in paths:
        with open(p) as f:
            reps.append(load_rift_csv(f))
    agg = aggregate_reps(reps)

    # An aggregate must not quietly rest on fewer samples than it appears to. A point missing from
    # some rep — a crashed run, a rep taken with different --sweep-connections, a truncated CSV —
    # would otherwise yield a complete-looking report whose cells are backed by 2 of 3 reps, with
    # exit 0 and nothing said. That is precisely the class of silence #773 exists to remove, so it
    # is a hard error rather than a warning.
    incomplete = {k: c["reps"] for k, c in agg.items() if c["reps"] != len(paths)}
    if incomplete:
        detail = ", ".join(f"{s}@c={c}/{m}: {n} of {len(paths)}"
                           for (s, c, m), n in sorted(incomplete.items())[:8])
        raise SystemExit(
            f"incomplete repetitions across {len(paths)} rep files for '{base_suffix}': {detail}"
            f"{' …' if len(incomplete) > 8 else ''}\n"
            f"Every point must appear in every rep, or the median silently rests on fewer samples "
            f"than the report claims. Re-run the missing reps, or aggregate a consistent subset.")

    csv_path = os.path.join(RESULTS_DIR, f"direct_rift{base_suffix}_median.csv")
    with open(csv_path, "w") as f:
        f.write(CSV_HEADER + ",reps,rps_spread_pct\n")
        for (scen, conns, mode), c in sorted(agg.items(), key=lambda kv: (kv[0][1], kv[0][0])):
            spread = f"{c['rps_spread_pct']:.1f}" if c["rps_spread_pct"] != "" else ""
            f.write(f"{csv_row(scen, conns, mode, c)},{c['reps']},{spread}\n")

    out = os.path.join(RESULTS_DIR, f"DIRECT_RIFT_MEDIAN_REPORT{base_suffix}.md")
    rep_counts = sorted({c["reps"] for c in agg.values()})
    with open(out, "w") as f:
        f.write("# Rift — Median of Repetitions (issue #773)\n\n")
        f.write(f"- **Date:** {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
        # The aggregation runs offline over CSVs that carry no version, so claiming one nobody
        # passed would put a wrong provenance line on the artefact meant to replace a retracted
        # number. Say "unspecified" instead of guessing.
        f.write(f"- **Rift:** {rift_ver or 'unspecified (pass --rift-version)'}\n")
        f.write(f"- **Variant:** `{base_suffix or '(none)'}`\n")
        f.write(f"- **Repetitions:** {', '.join(str(n) for n in rep_counts)} "
                f"(from {', '.join(os.path.basename(p) for p in paths)})\n")
        f.write("- **Spread** is peak-to-peak RPS as a percentage of the mean. A large spread "
                "means the reps disagree — treat the median as provisional and look at the "
                "individual reps before quoting it.\n\n")
        f.write("| Scenario | c | RPS (median) | spread | p50 | p99 | p999 | reps |\n")
        f.write("|---|--:|--:|--:|--:|--:|--:|--:|\n")
        lat = lambda v: "n/a" if v == "" else f"{v:g}"
        for (scen, conns, mode), c in sorted(agg.items(), key=lambda kv: (kv[0][1], kv[0][0])):
            spread = "n/a" if c["rps_spread_pct"] == "" else f"{c['rps_spread_pct']:.1f}%"
            f.write(f"| {scen} | {conns} | {c['rps']:,.0f} | {spread} | {lat(c['p50_ms'])} "
                    f"| {lat(c['p99_ms'])} | {lat(c['p999_ms'])} | {c['reps']} |\n")
    print(f"wrote {out}")
    return out

def run_all(duration, warmup, conn_list, rift_bin, mb_bin, engines, rate=None, recording=False,
            allocator=None, runtime=None, server_cpus=None, engine_cpus=None, gen_cpus=None,
            rep=None, quamina=None, stub_count=None, body_field_stubs=None):
    os.makedirs(RESULTS_DIR, exist_ok=True)
    node = shutil.which("node") or "node"
    csv_suffix = result_suffix(allocator, runtime, server_cpus, rep, quamina, stub_count,
                               body_field_stubs)
    engine_prefix, oha_prefix = taskset_prefix(engine_cpus), taskset_prefix(gen_cpus)
    full_plan = [
        ("rift", 0,   engine_prefix
                      + [rift_bin, "--port", str(admin_port_for(0)), "--allow-injection", "--loglevel", "warn"]
                      + runtime_launch_args(runtime)),
        ("mb",   100, engine_prefix
                      + [node, mb_bin, "start", "--port", str(admin_port_for(100)), "--allowInjection", "--loglevel", "warn"]),
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
            # RSS sampling (#717) likewise only makes sense for the rift process itself.
            bench(engine, admin_port_for(offset), offset, duration, warmup, conn_list,
                  rate=rate, recording=recording and engine == "rift",
                  pid=proc.pid if engine == "rift" else None, csv_suffix=csv_suffix,
                  oha_prefix=oha_prefix)
        finally:
            stop(proc, ports)
    rift_ver = subprocess.run([rift_bin, "--version"], capture_output=True, text=True).stdout.strip() or "local"
    if "rift" in engines and "mb" in engines:
        mb_ver = subprocess.run([node, mb_bin, "--version"], capture_output=True, text=True).stdout.strip() or "2.9.1"
        report(rift_ver, mb_ver, duration, conn_list[0])
    elif engines == ["rift"]:
        rift_only_report(rift_ver, duration, conn_list, rate, recording,
                          allocator=allocator, runtime=runtime, csv_suffix=csv_suffix,
                          engine_cpus=engine_cpus, gen_cpus=gen_cpus,
                          quamina=quamina, stub_count=stub_count,
                          binary_mb=binary_size_mb(rift_bin))
    else:
        # engines == ["mb"]: no comparison possible (needs Rift too) and no Rift-only report to write.
        print(f"[report] engines={engines}: benched only Mountebank; no report written "
              f"(the comparison needs both rift and mb)")

def rift_only_report(rift_ver, duration, conn_list, rate, recording, allocator=None,
                     runtime=None, csv_suffix="", engine_cpus=None, gen_cpus=None,
                     quamina=None, stub_count=None, binary_mb=None):
    """Rift-only sweep / open-loop report: scenario × (connections, mode) matrices of RPS and p999.
    The MB-comparison report is deliberately untouched so its historical single-point numbers stay
    comparable; this is the extra artefact the Turbo round consumes. Allocator bake-off runs (#717)
    keep their own suffixed CSV/report so mimalloc/jemalloc/system results never overwrite each other."""
    path = os.path.join(RESULTS_DIR, f"direct_rift{csv_suffix}.csv")
    with open(path) as f:
        rows = load_rift_csv(f)
    cols = list(dict.fromkeys((int(r["connections"]), r["mode"]) for r in rows))
    cell = {(r["scenario"], (int(r["connections"]), r["mode"])): r for r in rows}
    # Must mirror the order `bench()` runs them in, or a measured scenario is silently missing
    # from the report — the row would simply not be emitted, with nothing saying so.
    scen_order = [s[0] for s in SCENARIOS] + (
        [s[0] for s in DIMENSION_SCENARIOS] + [RECORDING_SCENARIO[0]] if recording else [])
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
    has_rss = any(r.get("rss_mb_peak", "") != "" for r in rows)
    out = os.path.join(RESULTS_DIR, f"DIRECT_RIFT_SWEEP_REPORT{csv_suffix}.md")
    with open(out, "w") as f:
        f.write("# Rift — Concurrency Sweep / Open-Loop (issue #702)\n\n")
        f.write(f"- **Date:** {time.strftime('%Y-%m-%d %H:%M:%S')}\n- **Rift:** {rift_ver}\n")
        if allocator:
            f.write(f"- **Allocator:** {allocator}\n")
        if runtime:
            f.write(f"- **Runtime:** {runtime}\n")
        if quamina:
            f.write(f"- **Body-field dimension (quamina):** {quamina} "
                    f"(verified from the binary's `Matching dimensions:` self-report)\n")
        if stub_count:
            f.write(f"- **Field-equals-on-body stubs:** {stub_count}\n")
        if binary_mb is not None:
            f.write(f"- **Binary size:** {binary_mb} MB\n")
        if engine_cpus:
            f.write(f"- **Engine CPUs:** {len(engine_cpus)} (`{cpuset_arg(engine_cpus)}`) — the "
                    f"worker count both topologies derive from `available_parallelism()`\n")
            f.write(f"- **Generator CPUs:** {len(gen_cpus)} (`{cpuset_arg(gen_cpus)}`) — disjoint "
                    f"physical cores, so oha never shares a core (or SMT sibling) with the engine\n")
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
        if has_rss:
            matrix(f, "RSS peak (MB, during measurement)", "rss_mb_peak", lat)
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
    ap.add_argument("--rift-bin", default=None,
                    help="path to a prebuilt rift-http-proxy binary; overrides --allocator's "
                         "own build (default: target/release/rift-http-proxy, or "
                         "target/alloc-<name>/release/rift-http-proxy when --allocator is set)")
    ap.add_argument("--mb-bin", default=os.path.expanduser("~/bench-mb/node_modules/mountebank/bin/mb"))
    ap.add_argument("--rift-version", default="local")
    ap.add_argument("--mb-version", default="2.9.1")
    ap.add_argument("--allocator", choices=list(ALLOCATORS),
                    help="build+bench one allocator variant (Rift-only; #717 bake-off). Builds "
                         "into target/alloc-<name>/ unless --rift-bin is given.")
    ap.add_argument("--runtime", choices=list(RUNTIME_MODES),
                    help="bench one runtime topology (Rift-only; #746 RFC-712 gate). The probe "
                         "verifies the binary's Runtime-topology self-report, so a per-core "
                         "request on macOS (which falls back) aborts instead of mislabeling.")
    ap.add_argument("--server-cores", type=int, metavar="N",
                    help="core-count axis (#746): confine the engine to N CPUs with taskset — "
                         "both topologies size their workers from it — and confine oha to the "
                         "remaining physical cores. N must fall on a core boundary; Linux only.")
    ap.add_argument("--quamina", choices=list(QUAMINA_VARIANTS),
                    help="build+bench one quamina body-field variant (Rift-only; #779). Builds "
                         "into target/quamina-<on|off>/ unless --rift-bin is given, and verifies "
                         "the binary's own `Matching dimensions:` self-report before benching — "
                         "the two variants match identically by design, so a mislabeled build "
                         "would not show up anywhere else in the results.")
    ap.add_argument("--stub-count", type=int, metavar="N",
                    help="number of field-equals-on-body stubs on the JSONBody imposter (#779; "
                         f"default {DEFAULT_JSON_BODY_STUBS}). The quamina dimension replaces an "
                         "O(N) scan, so its win is a function of N — sweep it (e.g. 10/100/1000) "
                         "rather than measuring one arbitrary point.")
    ap.add_argument("--body-field-stubs", type=int, metavar="N",
                    help=f"number of stubs sharing one path and discriminated only by a body field "
                         f"(#767/#779; default {DEFAULT_BODY_FIELD_STUBS}). This is the axis the "
                         f"quamina body-field dimension is measured on — unlike --stub-count, whose "
                         f"stubs have unique paths and are therefore pruned by the path dimension "
                         f"before the body matters.")
    ap.add_argument("--rep", type=int, metavar="N",
                    help="repetition index (#773): artefacts get a _repN suffix, so each rep of a "
                         "sweep lands in its own file instead of overwriting the last. Re-running "
                         "the SAME rep overwrites it, with a printed note. Rift-only. Without "
                         "--rep the run writes the unsuffixed name as before.")
    ap.add_argument("--aggregate-reps", metavar="SUFFIX",
                    help="offline: read direct_rift<SUFFIX>_rep*.csv and write the median-of-reps "
                         "CSV + report (per-rep spread included). SUFFIX is the variant part of "
                         "the name, e.g. '_per-core_cores8' (use '' for a bare run).")
    a = ap.parse_args()
    if a.aggregate_reps is not None:
        aggregate_reps_to_report(a.aggregate_reps, a.rift_version)
        sys.exit(0)
    if a.rep is not None and a.rep < 1:
        raise SystemExit(f"--rep must be >= 1 (got {a.rep})")
    if a.stub_count is not None and a.stub_count < 1:
        raise SystemExit(f"--stub-count must be >= 1 (got {a.stub_count})")
    if a.quamina and a.allocator:
        raise SystemExit(
            "--quamina and --allocator are separate bake-offs: combining them would attribute one "
            "variable's effect to the other. Run them as separate sweeps.")
    if a.stub_count is not None:
        set_json_body_stub_count(a.stub_count)
    if a.body_field_stubs is not None:
        if a.body_field_stubs < 1:
            raise SystemExit(f"--body-field-stubs must be >= 1 (got {a.body_field_stubs})")
        set_body_field_stub_count(a.body_field_stubs)
    if a.run_all:
        try:
            engines, conn_list, rate, recording = resolve_run_mode(
                a.engines, a.sweep_connections, a.open_loop, a.connections)
        except ValueError as e:
            raise SystemExit(str(e))
        requested = [e.strip() for e in a.engines.split(",") if e.strip()]
        # --rep is Rift-only, and refused rather than silently coerced: the rift-vs-mb comparison
        # report reads the UNSUFFIXED direct_{engine}.csv paths, so a repped rift+mb run would
        # write direct_rift_repN.csv and then build DIRECT_BENCHMARK_REPORT.md from whatever stale
        # unsuffixed file happened to be on disk — presenting numbers this run never measured,
        # which is the exact failure #773 exists to close. Coercing engines silently would hide
        # that the requested comparison was not run at all.
        if a.rep is not None and engines != ["rift"]:
            raise SystemExit(
                "--rep is Rift-only: the rift-vs-mb report reads unsuffixed artefacts, so a "
                "repped comparison run would report a stale file as this run's numbers. "
                "Re-run with --engines rift.")
        if (a.allocator or a.runtime or a.quamina) and engines != ["rift"]:
            engines = ["rift"]
        if engines != requested:
            print("note: sweep/open-loop/allocator is Rift-only; running --engines rift")
        rift_bin, needs_build = resolve_rift_bin(a.rift_bin, a.allocator, a.quamina)
        if needs_build:
            if a.quamina:
                build_quamina_binary(a.quamina)
            else:
                build_allocator_binary(a.allocator)
        if a.quamina:
            # Applies to an explicit --rift-bin too: the label on the results must come from the
            # binary, never from the invocation (#777).
            verify_quamina_marker(rift_bin, a.quamina)
        if a.allocator:
            # Applies to explicit --rift-bin AND harness-built binaries alike: the label on the
            # results must come from the binary itself, never from the invocation.
            verify_allocator_marker(rift_bin, a.allocator)
        if a.server_cores is not None and a.server_cores < 1:
            raise SystemExit(f"--server-cores must be >= 1 (got {a.server_cores})")
        engine_cpus, gen_cpus = resolve_cpu_split(a.server_cores)
        if engine_cpus:
            print(f"  cpu split: engine on {cpuset_arg(engine_cpus)} "
                  f"({len(engine_cpus)} CPUs), oha on {cpuset_arg(gen_cpus)} "
                  f"({len(gen_cpus)} CPUs)")
        if a.runtime:
            # Probe under the same pinning: per-core self-reports `per-core xN`, so this is also
            # what proves the core budget actually reached the engine's worker sizing.
            verify_runtime_marker(rift_bin, a.runtime, runtime_launch_args(a.runtime),
                                  prefix=taskset_prefix(engine_cpus),
                                  expected_workers=a.server_cores)
        run_all(a.duration, a.warmup, conn_list, rift_bin, a.mb_bin, engines,
                rate=rate, recording=recording, allocator=a.allocator, runtime=a.runtime,
                server_cpus=a.server_cores, engine_cpus=engine_cpus, gen_cpus=gen_cpus,
                rep=a.rep, quamina=a.quamina, stub_count=a.stub_count,
                body_field_stubs=a.body_field_stubs)
    elif a.report:
        report(a.rift_version, a.mb_version, a.duration, a.connections)
    else:
        raise SystemExit("use --run-all (or --report)")
