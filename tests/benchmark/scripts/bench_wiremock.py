#!/usr/bin/env python3
"""WireMock benchmark suite (issue #861) — the third engine in the published comparison.

`bench_direct.py` compares Rift against Mountebank on byte-identical imposter JSON. WireMock cannot
consume that JSON: it has its own stub-mapping schema and a different process model (one port per
JVM, admin under `/__admin` on that same port). So this is a **separate suite** rather than a third
engine inside `bench_direct.py`, for one specific reason: the rift-vs-mb default run is a stability
contract (`DefaultRunUnchanged`) that must stay byte-comparable with previously published numbers,
and leaving `bench_direct.py` untouched makes that risk zero by construction.

The two suites still share one source of truth. This module **imports** the canonical fixture
(`IMPOSTERS`, `SCENARIOS`, `EXPECT_BODY`) and the shared plumbing (`run_oha`/`metric`,
`verify_body`, `launch`/`stop`/`free_ports`, the CSV writers) from `bench_direct`; WireMock
mappings are *generated* from `IMPOSTERS` by the translator below. Hand-written parallel fixtures
would drift, and a drifted fixture publishes a number that compares two different workloads.

Known fidelity gaps, recorded rather than hidden — none affects a matched request in the current
fixture, but a future generator could walk into one:

- **Case sensitivity.** Mountebank/Rift predicates are case-*insensitive* by default; WireMock's
  `equalTo`/`contains`/`urlPath` are case-sensitive. Every request the fixture measures matches
  exactly, and the difference makes WireMock do strictly *less* string work, so it cannot flatter
  Rift — but it is not like-for-like.
- **`complex_predicate` scans twice as far** under WireMock (see `wiremock_mappings`), which the
  generated report states in its own caveat.

Run:
    python3 bench_wiremock.py --run-all          # bench WireMock alone -> direct_wiremock.csv
    python3 bench_wiremock.py --report           # combine whatever CSVs exist -> 3-way report

Prerequisites: a JRE (WireMock 3 needs 11+; use an LTS, 17 or 21) and the WireMock standalone jar.
    mkdir -p ~/bench-wiremock && curl -Lo ~/bench-wiremock/wiremock-standalone.jar \\
      https://repo1.maven.org/maven2/org/wiremock/wiremock-standalone/3.9.1/wiremock-standalone-3.9.1.jar
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(__file__))
from bench_direct import (  # noqa: E402
    IMPOSTERS, RESULTS_DIR, SCENARIOS,
    free_ports, launch, load_rift_csv, metric, mode_label,
    parse_conn_list, run_oha, stop, verify_body, write_results_csv,
)

# rift = offset 0, mb = offset 100 (bench_direct), wiremock = offset 200. Disjoint ranges are what
# let the engines run one at a time without cross-talk; 200 keeps 4745-4760 clear of both.
WIREMOCK_OFFSET = 200

# WireMock resolves competing stubs by `priority` (lower wins), falling back to most-recently-added.
# The catch-all must lose to every translated mapping, so it sits at the far end of that ordering.
CATCH_ALL_PRIORITY = 999_999

MIN_JAVA_MAJOR = 11
DEFAULT_JAR = os.path.expanduser("~/bench-wiremock/wiremock-standalone.jar")
DEFAULT_WIREMOCK_VERSION = "3.9.1"

# The stock-defaults secondary series (issue #865): same suite, WireMock launched with no
# `--container-threads`, benched at the headline connection count only. Its own engine label keeps
# it in its own CSV and out of the headline Rift/WireMock ratio, while still being reportable
# beside it — the out-of-the-box story as secondary data, not the number.
STOCK_ENGINE = "wiremock-stock"

# WireMock's documented Jetty container-pool default. Reported next to the tuned value so a reader
# can tell a thread-pool ceiling from an engine throughput ceiling.
STOCK_CONTAINER_THREADS = 10


# --------------------------------------------------------------------------------------------
# Stub translator
# --------------------------------------------------------------------------------------------

def _fail(msg):
    """Abort translation.

    Deliberately fatal rather than "skip the stub I don't understand": a silently dropped stub still
    passes most scenarios and corrupts exactly the one it mattered for, which is the failure mode
    that publishes an inflated number. `verify_body` would catch it for the 13 measured scenarios,
    but not for the hundreds of stubs that merely make the match *work harder*."""
    raise SystemExit(f"bench_wiremock: cannot translate stub — {msg}")


def _value_matcher_headers(fields, kind, out):
    headers = out.setdefault("headers", {})
    for k, v in fields.items():
        headers[k] = {kind: v}


def _set_url_path(out, key, value):
    # WireMock takes exactly one URL matcher per request; two would silently drop one.
    for other in ("urlPath", "urlPathPattern", "urlPattern", "url"):
        if other in out and other != key:
            _fail(f"two URL matchers on one mapping ({other} and {key})")
    out[key] = value


def _apply_equals_like(op, fields, out, strict_body):
    for field, value in fields.items():
        # `deepEquals` means "these key/value pairs and NO others" (see rift's own
        # predicate/deep_equals.rs). WireMock's `queryParameters`/`headers` are *subset* matchers
        # with no way to say "and nothing else", so there is no faithful translation — and an
        # unfaithful one is worse than none here, because it would also be strictly *cheaper* than
        # the exact-set check the other engines run, quietly flattering WireMock. The body branch
        # below is the only field where the distinction is expressible (`ignoreExtraElements`).
        if op == "deepEquals" and field in ("headers", "query"):
            _fail(f"deepEquals.{field} has no faithful WireMock translation: "
                  f"queryParameters/headers are subset matchers and cannot express "
                  f"'and no other keys'. Translating it as a subset would measure a weaker "
                  f"(and cheaper) predicate than Rift/Mountebank evaluate.")
        if field == "method":
            out["method"] = value
        elif field == "path":
            _set_url_path(out, "urlPath", value)
        elif field == "headers":
            _value_matcher_headers(value, "equalTo", out)
        elif field == "query":
            params = out.setdefault("queryParameters", {})
            for k, v in value.items():
                params[k] = {"equalTo": v}
        elif field == "body":
            if not isinstance(value, dict):
                _fail(f"{op}.body must be a JSON object here, got {type(value).__name__} "
                      f"({value!r}); a scalar body only appears under a jsonpath selector")
            pattern = {"equalToJson": json.dumps(value, separators=(",", ":"))}
            if not strict_body:
                # Mountebank's `equals` on a parsed JSON body tolerates extra fields; `deepEquals`
                # does not. Mirroring that distinction is what keeps the workloads comparable.
                pattern["ignoreExtraElements"] = True
            out.setdefault("bodyPatterns", []).append(pattern)
        else:
            _fail(f"unsupported field {field!r} under {op!r}")


def _apply_operator(op, fields, out):
    if op == "equals":
        _apply_equals_like(op, fields, out, strict_body=False)
    elif op == "deepEquals":
        _apply_equals_like(op, fields, out, strict_body=True)
    elif op == "matches":
        for field, value in fields.items():
            if field != "path":
                _fail(f"unsupported field {field!r} under 'matches'")
            # MB `matches` is an unanchored regex *search*; `urlPathPattern` anchors the full
            # path. Every pattern in the fixture spans the whole path, so the two agree today. The
            # leading-'/' check below is a cheap prefix sanity check, NOT a proof of equivalence:
            # a pattern that is anchored at the start but open at the end (`/v1/[0-9]+`) still
            # matches `/v1/123/details` under MB and not under WireMock. Deciding that in general
            # is not tractable here, so the guard catches the obvious case and this comment owns
            # the rest — revisit if a generator ever adds a genuinely partial-path pattern.
            if not value.startswith("/"):
                _fail(f"'matches.path' pattern {value!r} is not anchored at '/': WireMock's "
                      f"urlPathPattern matches the whole path, so translating an unanchored "
                      f"MB search would change its meaning")
            _set_url_path(out, "urlPathPattern", value)
    elif op == "startsWith":
        for field, value in fields.items():
            if field != "path":
                _fail(f"unsupported field {field!r} under 'startsWith'")
            _set_url_path(out, "urlPathPattern", re.escape(value) + ".*")
    elif op == "contains":
        for field, value in fields.items():
            if field == "path":
                _set_url_path(out, "urlPathPattern", ".*" + re.escape(value) + ".*")
            elif field == "headers":
                _value_matcher_headers(value, "contains", out)
            else:
                _fail(f"unsupported field {field!r} under 'contains'")
    else:
        _fail(f"unsupported predicate operator {op!r}")


def _apply_selector_predicate(pred, out):
    """A predicate carrying a `jsonpath`/`xpath` selector: the operator applies to the *selected*
    value, not the raw body, so it becomes a body pattern rather than a request field."""
    if "jsonpath" in pred:
        selector = pred["jsonpath"].get("selector")
        if selector is None:
            _fail("jsonpath predicate without a selector")
        ops = [k for k in pred if k not in ("jsonpath", "caseSensitive")]
        if ops != ["equals"] or list(pred["equals"]) != ["body"]:
            _fail(f"jsonpath selector supports only `equals.body`, got {ops} / {list(pred.get('equals', {}))}")
        out.setdefault("bodyPatterns", []).append(
            {"matchesJsonPath": {"expression": selector, "equalTo": pred["equals"]["body"]}})
        return
    selector = pred["xpath"].get("selector")
    if selector is None:
        _fail("xpath predicate without a selector")
    ops = [k for k in pred if k not in ("xpath", "caseSensitive")]
    if ops != ["exists"] or pred["exists"] != {"body": True}:
        _fail(f"xpath selector supports only `exists.body: true`, got {ops} / {pred.get('exists')}")
    out.setdefault("bodyPatterns", []).append({"matchesXPath": selector})


def _apply_predicate(pred, variants):
    """Fold one imposter predicate into every request variant, returning the new variant list.

    Variants exist for `or`: WireMock's criteria within one mapping are implicitly ANDed and its
    `or` operator combines *value matchers against a single value*, not criteria across different
    fields. `complex_stubs` needs an OR over two different headers, so the only faithful
    translation is one mapping per branch. Folding over a list and cross-producting on `or` makes
    the nested `and[..., or[...]]` shape fall out without a special case."""
    if "jsonpath" in pred or "xpath" in pred:
        for v in variants:
            _apply_selector_predicate(pred, v)
        return variants

    ops = list(pred)
    if len(ops) != 1:
        _fail(f"expected exactly one operator per predicate, got {ops}")
    op = ops[0]

    if op == "and":
        for sub in pred["and"]:
            variants = _apply_predicate(sub, variants)
        return variants

    if op == "or":
        branches = pred["or"]
        if not branches:
            _fail("empty `or` predicate")
        out = []
        for branch in branches:
            # Each branch gets its own deep copy of every variant so far.
            out.extend(_apply_predicate(branch, [json.loads(json.dumps(v)) for v in variants]))
        return out

    for v in variants:
        _apply_operator(op, pred[op], v)
    return variants


def _translate_response(stub):
    responses = stub.get("responses", [])
    if len(responses) != 1:
        _fail(f"expected exactly one response per stub, got {len(responses)}")
    resp = responses[0]
    if list(resp) != ["is"]:
        _fail(f"only `is` responses are translatable, got {list(resp)}")
    is_ = resp["is"]
    unknown = set(is_) - {"statusCode", "headers", "body"}
    if unknown:
        _fail(f"unsupported `is` response fields {sorted(unknown)}")
    out = {"status": is_.get("statusCode", 200)}
    if "headers" in is_:
        out["headers"] = dict(is_["headers"])
    if "body" in is_:
        out["body"] = is_["body"]
    return out


def catch_all_mapping():
    """Rift/MB answer an unmatched request with an empty 200; WireMock answers 404 with diagnostic
    text. Without this, `no_match` would fail its empty-body assertion AND trip the all-2xx gate —
    so the comparison needs WireMock to adopt the other engines' no-match behaviour."""
    return {
        "priority": CATCH_ALL_PRIORITY,
        "request": {"urlPattern": ".*"},
        "response": {"status": 200, "body": ""},
    }


def wiremock_mappings(stubs):
    """Translate imposter stubs into WireMock mappings, preserving first-match-wins.

    Rift/MB take the first matching stub in declaration order. WireMock resolves by `priority`
    (lower wins) and otherwise by most-recently-added — so equal priorities would *invert* the
    intended order. The counter below is therefore strictly increasing across emitted mappings,
    including across an `or`-split; it coincides with the 1-based stub index until the first split.
    """
    out = []
    priority = 0
    for stub in stubs:
        predicates = stub.get("predicates", [])
        variants = [{}]
        for pred in predicates:
            variants = _apply_predicate(pred, variants)
        response = _translate_response(stub)
        for request in variants:
            priority += 1
            if priority >= CATCH_ALL_PRIORITY:
                _fail(f"too many mappings ({priority}); would collide with the catch-all priority")
            out.append({"priority": priority, "request": request,
                        "response": json.loads(json.dumps(response))})
    out.append(catch_all_mapping())
    return out


# --------------------------------------------------------------------------------------------
# Java preflight + process model
# --------------------------------------------------------------------------------------------

def parse_java_major(version_output):
    """Major version from `java -version` output, or None if unparseable.

    Handles both the legacy `1.8.0_x` spelling and the modern `17.0.13` one."""
    m = re.search(r'version "(\d+)(?:\.(\d+))?', version_output)
    if not m:
        return None
    major = int(m.group(1))
    if major == 1 and m.group(2):
        return int(m.group(2))
    return major


def java_preflight():
    """Fail once, actionably, instead of as 14 cryptic instance-launch failures."""
    try:
        out = subprocess.run(["java", "-version"], capture_output=True, text=True, timeout=30)
    except FileNotFoundError:
        raise SystemExit(
            "bench_wiremock: `java` not found. WireMock 3 needs a JRE 11+ (use an LTS, 17 or 21). "
            "This suite is native-process by design, so the official WireMock container is not a "
            "substitute.")
    text = (out.stderr or "") + (out.stdout or "")
    major = parse_java_major(text)
    if major is None:
        raise SystemExit(f"bench_wiremock: could not parse `java -version` output: {text.strip()[:200]}")
    if major < MIN_JAVA_MAJOR:
        raise SystemExit(
            f"bench_wiremock: java {major} is too old — WireMock 3 needs {MIN_JAVA_MAJOR}+ "
            f"(use an LTS, 17 or 21).")
    return major, text.strip().splitlines()[0] if text.strip() else f"java {major}"


def instance_ports():
    return [port + WIREMOCK_OFFSET for port, _, _ in IMPOSTERS]


def container_thread_count(conn_list, override=None):
    """How many Jetty container threads the *tuned* series runs with (issue #865).

    WireMock is thread-per-request, so in-flight concurrency is bounded by this pool — and it
    defaults to 10, while the comparison drives 50 connections. Measuring that configuration
    measures the pool, not the engine, and "WireMock was throttled to 10 in-flight" is the first
    rebuttal any WireMock user raises, because unlike Mountebank's architecturally single-threaded
    Node this is a one-flag tune.

    `max(cpu_count, max(connections))` is the defensible pin: pinning to core count would still
    throttle WireMock below the offered concurrency (same criticism, different number), while this
    guarantees the pool is never the bottleneck. Extra threads do not manufacture parallelism — the
    CPU budget is unchanged — they only remove the artificial queue, so a Rift win here is
    unambiguous. When sweeping, the TOP of the sweep is what must not be throttled."""
    if override is not None:
        if override < 1:
            raise SystemExit(f"bench_wiremock: --container-threads must be >= 1, got {override}")
        return override
    return max(os.cpu_count() or 1, max(conn_list))


def wiremock_cmd(jar, port, container_threads=None):
    """WireMock's argv for one imposter port.

    `container_threads=None` means *stock defaults* — no `--container-threads` flag at all, which
    is exactly what the secondary `wiremock-stock` series measures. Any int pins the pool.

    `--no-request-journal` is a fairness requirement, not a tuning knob. WireMock records every
    incoming request by default and the journal is unbounded, so a JVM under 30s of load retains
    hundreds of thousands of `LoggedRequest` objects (headers and body copied). Rift and Mountebank
    are measured with recording OFF — `bench_direct.load_imposters` posts no `recordRequests`, and
    journaling is a deliberately *separate*, additive, Rift-only scenario (`RECORDING_SCENARIO`)
    precisely because it is a distinct cost. Leaving WireMock's journal on would compare
    WireMock-with-recording against Rift-without, and would also make the four scenarios sharing
    the API instance non-comparable with each other, since each is measured against a progressively
    fuller journal. Nothing in this suite reads `/__admin/requests`, so disabling it costs nothing.
    """
    cmd = ["java", "-jar", jar, "--port", str(port), "--disable-banner",
           "--no-request-journal"]
    if container_threads is not None:
        cmd += ["--container-threads", str(container_threads)]
    return cmd


def admin_ready(port, tries=240):
    """Readiness is `GET /__admin/mappings` answering 200 — there is no separate 2525-style admin
    port, and a bare TCP accept would race the admin servlet coming up."""
    url = f"http://localhost:{port}/__admin/mappings"
    for _ in range(tries):
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def load_mappings(port, mappings):
    """Bulk-import one instance's mappings.

    `/__admin/mappings/import` in one request rather than a POST per mapping: the fixture is ~1000
    stubs across 14 instances, so per-mapping POSTs would be ~14k round trips of pure setup."""
    body = json.dumps({"mappings": mappings}).encode()
    req = urllib.request.Request(f"http://localhost:{port}/__admin/mappings/import",
                                 data=body, method="POST",
                                 headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req, timeout=120).close()
    except urllib.error.HTTPError as e:
        # urlopen raises on >=400, so this is the real failure path — a bare traceback here would
        # bury a stub-validation rejection (422) that the operator needs to read.
        detail = e.read().decode("utf-8", "replace")[:400]
        raise SystemExit(f"bench_wiremock: mapping import failed on {port}: "
                         f"HTTP {e.code}: {detail}")

    # Verify the count landed. An import that silently drops a mapping is the same class of failure
    # `_fail` exists to prevent: the 13 measured scenarios would still pass while the surrounding
    # stubs — the ones that make the match *work* — quietly went missing, inflating the number.
    with urllib.request.urlopen(f"http://localhost:{port}/__admin/mappings?limit=0",
                                timeout=60) as r:
        total = json.loads(r.read()).get("meta", {}).get("total")
    if total != len(mappings):
        raise SystemExit(f"bench_wiremock: {port} reports {total} mappings, expected "
                         f"{len(mappings)} — the import dropped some; measuring this would be bogus")


def probe_wiremock_version(port, fallback):
    """WireMock does not guarantee an admin version endpoint across 2.x/3.x, so treat it as
    best-effort and fall back to the pinned CLI value rather than failing a whole run over a
    reporting detail."""
    try:
        with urllib.request.urlopen(f"http://localhost:{port}/__admin/version", timeout=5) as r:
            text = r.read().decode("utf-8", "replace").strip()
        if text:
            try:
                parsed = json.loads(text)
                return parsed.get("version", text) if isinstance(parsed, dict) else text
            except json.JSONDecodeError:
                return text
    except Exception:
        pass
    return fallback


def launch_instances(jar, logdir, procs, container_threads=None):
    """Spawn one JVM per imposter and wait for each to answer on its admin path.

    `procs` is an out-parameter the caller owns, appended to as each process is spawned. That is
    deliberate: if instance 3 never becomes ready, the caller's `finally` still has to see
    instances 1..14 to shut them down. Returning the list instead would strand every already-spawned
    JVM on the readiness failure — and a JVM still in startup is not yet listening, so `free_ports`
    cannot rescue it either; it would bind its port moments later and poison the next run."""
    os.makedirs(logdir, exist_ok=True)
    # All 14, including the four (DeepEquals/Literal/MethodMix/BodyField) that only appear in
    # bench_direct's Rift-only DIMENSION_SCENARIOS: rift and mb hold those imposters during their
    # runs too, so loading them keeps the resident-stub count comparable across engines.
    for (base_port, name, stubs) in IMPOSTERS:
        port = base_port + WIREMOCK_OFFSET
        log = os.path.join(logdir, f"wiremock-{name}-{port}.log")
        procs.append((name, port, launch(wiremock_cmd(jar, port, container_threads), log),
                      log, stubs))
    for i, (name, port, _proc, log, _stubs) in enumerate(procs, 1):
        if not admin_ready(port):
            raise SystemExit(f"bench_wiremock: instance {name} on {port} never became ready "
                             f"(see {log})")
        print(f"    [{i}/{len(procs)}] {name} ready on {port}")
    return procs


def load_all_mappings(procs):
    for name, port, _proc, _log, stubs in procs:
        mappings = wiremock_mappings(stubs)
        t0 = time.time()
        load_mappings(port, mappings)
        print(f"    {name} port={port} mappings={len(mappings)} in {time.time() - t0:.2f}s")


def stop_instances(procs):
    # `bench_direct.stop` raises if a port is still bound after its drain window. Let that abort the
    # whole teardown and every *later* instance leaks, along with the caller's `free_ports` sweep —
    # a window this suite now opens twice per run. Collect and re-raise instead, so each JVM gets
    # its stop attempt first.
    failures = []
    for name, port, proc, _log, _stubs in procs:
        try:
            stop(proc, [port])
        except SystemExit as e:
            failures.append(f"{name}:{port} ({e})")
    if failures:
        raise SystemExit(f"bench_wiremock: instances failed to stop: {'; '.join(failures)}")


# --------------------------------------------------------------------------------------------
# Bench loop
# --------------------------------------------------------------------------------------------

def bench_wiremock(duration, warmup, conn_list, csv_suffix="", engine="wiremock"):
    os.makedirs(RESULTS_DIR, exist_ok=True)
    mode = mode_label(None)
    rows = []
    for conns in conn_list:
        for name, base_port, method, path, body, headers in SCENARIOS:
            url = f"http://localhost:{base_port + WIREMOCK_OFFSET}{path}"
            # The same two gates bench_direct uses, and the reason a wrong translation cannot be
            # published as a fast number: the body marker proves the intended mapping served the
            # request, and a non-2xx distribution aborts the run.
            verify_body(engine, name, method, url, body, headers)
            run_oha(url, method, body, headers, warmup, conns)
            m = metric(run_oha(url, method, body, headers, duration, conns))
            total = sum(m["codes"].values())
            good = all(c.startswith("2") for c in m["codes"])
            status = "ok" if good and total > 0 else f"BAD codes={m['codes']}"
            print(f"  {name:20s} c={conns:<4d} {m['rps']:>10.1f} rps  "
                  f"p50={m['p50_ms']}ms p99={m['p99_ms']}ms p999={m['p999_ms']}ms  {status}")
            if not (good and total > 0):
                raise SystemExit(f"{engine}/{name}: unexpected status distribution {m['codes']} — aborting")
            rows.append((name, conns, mode, m))
    path = write_results_csv(engine, csv_suffix, rows)
    print(f"[{engine}] wrote {path}")
    return path


def _run_series(jar, logdir, ports, container_threads, duration, warmup, conn_list,
                csv_suffix, engine, wiremock_version):
    """Launch, load, bench and tear down one WireMock configuration.

    Container threads are a JVM start flag, so the tuned and stock series cannot share a launch —
    each needs its own pass. `procs` is owned here so the `finally` still stops every JVM that was
    spawned when a later one fails its readiness probe."""
    label = ("stock defaults" if container_threads is None
             else f"{container_threads} container threads")
    print(f"[{engine}] launching {len(IMPOSTERS)} instances at offset +{WIREMOCK_OFFSET} ({label})")
    procs = []
    try:
        launch_instances(jar, logdir, procs, container_threads)
        print(f"[{engine}] loading mappings")
        load_all_mappings(procs)
        version = probe_wiremock_version(procs[0][1], wiremock_version)
        print(f"[{engine}] version {version}")
        bench_wiremock(duration, warmup, conn_list, csv_suffix, engine=engine)
    finally:
        stop_instances(procs)
        free_ports(ports)


def run_all(jar, duration, warmup, conn_list, csv_suffix="",
            wiremock_version=DEFAULT_WIREMOCK_VERSION, container_threads=None,
            stock_conns=50):
    """Bench WireMock twice: the tuned headline series, then the stock-default secondary series.

    The stock series runs at the headline connection count only — it exists to tell the
    out-of-the-box story, not to be swept — and is written under its own engine label so the report
    can show it beside the headline without it entering the Rift/WireMock ratio (issue #865)."""
    major, java_line = java_preflight()
    print(f"[wiremock] java {major} ({java_line})")
    if not os.path.exists(jar):
        raise SystemExit(
            f"bench_wiremock: WireMock jar not found at {jar}. Download it with:\n"
            f"  mkdir -p {os.path.dirname(jar)} && curl -Lo {jar} \\\n"
            f"    https://repo1.maven.org/maven2/org/wiremock/wiremock-standalone/"
            f"{wiremock_version}/wiremock-standalone-{wiremock_version}.jar")

    threads = container_thread_count(conn_list, container_threads)
    record_container_threads(csv_suffix, threads)
    ports = instance_ports()
    logdir = os.path.join(RESULTS_DIR, "logs")
    free_ports(ports)

    _run_series(jar, logdir, ports, threads, duration, warmup, conn_list,
                csv_suffix, "wiremock", wiremock_version)

    # Stock defaults, headline connection count only — the out-of-the-box story as secondary data.
    _run_series(jar, logdir, ports, None, duration, warmup, [stock_conns],
                csv_suffix, STOCK_ENGINE, wiremock_version)
    return threads


# --------------------------------------------------------------------------------------------
# Report
# --------------------------------------------------------------------------------------------

def threads_sidecar_path(csv_suffix):
    return os.path.join(RESULTS_DIR, f"direct_wiremock{csv_suffix}.threads")


def record_container_threads(csv_suffix, threads):
    """Persist the pin the tuned series actually ran with, beside its CSV.

    The documented flow is two commands — `--run-all`, then a separate `--report` — so the report
    process does not know what the run chose. Recomputing it from the *report* invocation's flags
    would silently print a different number than the one measured (e.g. run with
    `--sweep-connections 50,200` pins 200; `--report --connections 50` would infer 50). Requirement
    4 of issue #865 is that the report state the value **actually used**, so it is recorded rather
    than re-derived."""
    with open(threads_sidecar_path(csv_suffix), "w") as f:
        f.write(f"{threads}\n")


def recorded_container_threads(csv_suffix):
    """The pin the tuned series ran with, or None if it was never recorded."""
    try:
        with open(threads_sidecar_path(csv_suffix)) as f:
            return int(f.read().strip())
    except (OSError, ValueError):
        return None


def engine_csv_path(engine, csv_suffix):
    """Where `--report` looks for an engine's CSV.

    Deliberately resolved against *this* module's `RESULTS_DIR` rather than delegating to
    `bench_direct.results_csv_path`, which closes over its own copy. The two are the same directory
    in a real run — the bench half writes through `write_results_csv` — but routing every read
    through one local seam is what makes the combiner testable against a temp directory instead of
    whatever a previous benchmark left in `results/`."""
    return os.path.join(RESULTS_DIR, f"direct_{engine}{csv_suffix}.csv")


def load_engine_csv(engine, csv_suffix, conns):
    """Rows for one engine at the single closed-loop comparison point, or None if absent.

    Filtering on mode+connections is the stale-artefact guard: a CSV left behind by a sweep or an
    open-loop run holds rows that are not comparable, and blending them into the headline table
    would be silently wrong."""
    path = engine_csv_path(engine, csv_suffix)
    if not os.path.exists(path):
        return None
    with open(path) as f:
        rows = {r["scenario"]: {"rps": float(r["rps"]), "p50": r["p50_ms"], "p99": r["p99_ms"]}
                for r in load_rift_csv(f)
                if r["mode"] == "closed" and int(r["connections"]) == conns}
    return rows or None


def report(rift_ver, mb_ver, wiremock_ver, java_ver, duration, conns, csv_suffix="",
           container_threads=None):
    """Combine the CSVs into the 3-way table.

    `container_threads` is the pin the tuned series ran with, stated in the Method section so a
    reader can tell a thread-pool ceiling from an engine throughput ceiling (issue #865). The
    stock-defaults series is rendered as its own clearly-labelled column and is deliberately NOT
    used for the Rift/WM speedup — that ratio must come from the un-throttled run."""
    rift = load_engine_csv("rift", csv_suffix, conns)
    wm = load_engine_csv("wiremock", csv_suffix, conns)
    stock = load_engine_csv(STOCK_ENGINE, csv_suffix, conns)
    mb = load_engine_csv("mb", csv_suffix, conns)
    if rift is None:
        raise SystemExit(f"bench_wiremock: no comparable rift rows at connections={conns}, "
                         f"mode=closed in {engine_csv_path('rift', csv_suffix)}")
    if wm is None:
        raise SystemExit(f"bench_wiremock: no comparable wiremock rows at connections={conns}, "
                         f"mode=closed in {engine_csv_path('wiremock', csv_suffix)}")

    order = [s[0] for s in SCENARIOS]
    out = os.path.join(RESULTS_DIR, f"WIREMOCK_BENCHMARK_REPORT{csv_suffix}.md")
    with open(out, "w") as f:
        f.write("# Rift vs WireMock vs Mountebank — Direct-Process Benchmark\n\n")
        f.write(f"- **Date:** {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
        f.write(f"- **Rift:** {rift_ver}\n")
        f.write(f"- **WireMock:** {wiremock_ver} (JVM: {java_ver})\n")
        f.write(f"- **Mountebank:** {mb_ver if mb else 'not measured on this box'}\n")
        f.write(f"- **Load generator:** oha, {conns} keep-alive connections, {duration} per scenario "
                f"(after warmup)\n")
        f.write("- **Method:** native processes (no Docker); engines run one at a time on disjoint "
                "port ranges; the same 13 scenarios and the same oha settings for every engine; "
                "each scenario's matched body is asserted before it is measured.\n")
        tuned = (f"**{container_threads}** Jetty container threads" if container_threads is not None
                 else "an **unrecorded** number of Jetty container threads "
                      "(pass --container-threads to record it)")
        f.write(f"- **WireMock setup:** one JVM per imposter port (14 instances), stock heap/GC, and "
                f"{tuned} for the headline series. Response templating "
                f"is **off**, so `${{request.path}}` is served literally exactly as Mountebank "
                f"serves it. The request journal is **off** (`--no-request-journal`): it is on and "
                f"unbounded by default, and Rift and Mountebank are both measured with recording "
                f"off — journaling is a separate, additive scenario in the Rift suite precisely "
                f"because it is a distinct cost.\n")
        f.write(f"- **Why the pool is pinned.** WireMock is thread-per-request, so its default "
                f"{STOCK_CONTAINER_THREADS}-thread pool bounds in-flight requests at "
                f"{STOCK_CONTAINER_THREADS}, below the {conns} connections offered here. That "
                f"makes the pool a possible ceiling with nothing to do with the engine, so the "
                f"headline series pins it above the offered concurrency and the number measures "
                f"WireMock on the same CPU budget Rift gets. **The pin is a fairness guarantee, "
                f"not a speedup:** extra threads do not manufacture parallelism, they only remove "
                f"an artificial queue. On a box with about as many cores as the default pool has "
                f"threads, the CPU saturates first and the pin is a no-op — compare the two "
                f"WireMock columns to see whether it bound on this run.\n")
        if stock:
            f.write(f"- **The `WireMock (stock)` column is secondary data**, measured at WireMock's "
                    f"out-of-the-box {STOCK_CONTAINER_THREADS}-thread default at "
                    f"{conns} connections. It is what a user gets before tuning, and it is "
                    f"deliberately **not** the basis of the Rift/WM ratio. If it reads within "
                    f"noise of the headline column, the {STOCK_CONTAINER_THREADS}-thread default "
                    f"was not the binding constraint on this hardware. It also runs *after* the "
                    f"headline series in the same session, so treat small differences between the "
                    f"two WireMock columns as ordering noise rather than signal.\n")
        f.write("- **Read `complex_predicate` with care.** WireMock cannot express an OR across two "
                "*different* headers in one stub, so that imposter's 50 stubs become 101 mappings "
                "and the measured request matches the 50th candidate where Rift/Mountebank match "
                "the 25th. Its ratio therefore reflects roughly twice the candidate scan, not "
                "predicate cost alone — a real cost of modelling this workload in WireMock, but not "
                "a like-for-like predicate comparison.\n")
        f.write("- **The status-distribution gate is weaker here than for Rift/MB.** WireMock 404s "
                "an unmatched request, so this suite installs a catch-all empty-200 to reproduce "
                "the other engines' no-match default — which also means an all-2xx distribution no "
                "longer proves a match. The per-scenario body-marker assertion is the gate that "
                "does.\n\n")

        stock_head = f" WireMock (stock, {STOCK_CONTAINER_THREADS}t) |" if stock else ""
        stock_rule = " --:|" if stock else ""
        # Two adjacent WireMock columns where only one carries its thread count would read as
        # "stock vs default" — the exact confusion #865 exists to remove.
        wm_head = f"WireMock ({container_threads}t)" if container_threads is not None else "WireMock"
        f.write("## Throughput (requests/sec, higher is better)\n\n")
        f.write(f"| Scenario | Mountebank |{stock_head} {wm_head} | Rift | Rift/MB | Rift/WM |\n")
        f.write(f"|---|--:|{stock_rule}--:|--:|--:|--:|\n")
        for name in order:
            wr, rr = wm[name]["rps"], rift[name]["rps"]
            mr = mb[name]["rps"] if mb else None
            mb_cell = f"{mr:,.0f}" if mr is not None else "n/a"
            mb_sp = f"{rr / mr:.1f}x" if mr else "n/a"
            # Deliberately the TUNED series: a speedup over a 10-thread-throttled WireMock would
            # measure the pool, and is the first thing a WireMock user would (rightly) reject.
            wm_sp = f"{rr / wr:.1f}x" if wr else "n/a"
            stock_cell = ""
            if stock:
                sr = stock.get(name, {}).get("rps")
                stock_cell = f" {sr:,.0f} |" if sr is not None else " n/a |"
            f.write(f"| {name} | {mb_cell} |{stock_cell} {wr:,.0f} | {rr:,.0f} | "
                    f"**{mb_sp}** | **{wm_sp}** |\n")

        f.write("\n## Latency p99 (ms, lower is better)\n\n")
        f.write(f"| Scenario | Mountebank |{stock_head} {wm_head} | Rift |\n")
        f.write(f"|---|--:|{stock_rule}--:|--:|\n")
        for name in order:
            mb_cell = mb[name]["p99"] if mb else "n/a"
            stock_cell = f" {stock.get(name, {}).get('p99', 'n/a')} |" if stock else ""
            f.write(f"| {name} | {mb_cell} |{stock_cell} {wm[name]['p99']} | {rift[name]['p99']} |\n")
    print(f"wrote {out}")
    return out


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description="WireMock benchmark suite (issue #861)")
    ap.add_argument("--run-all", action="store_true", help="bench WireMock alone")
    ap.add_argument("--report", action="store_true",
                    help="combine direct_rift.csv / direct_mb.csv / direct_wiremock.csv from the "
                         "SAME box and settings into WIREMOCK_BENCHMARK_REPORT.md")
    ap.add_argument("--duration", default="20s")
    ap.add_argument("--warmup", default="10s",
                    help="3s is thin for a JIT; 10s is the recommended default for WireMock runs. "
                         "Quote rift/mb numbers measured with the same warmup for comparability.")
    ap.add_argument("--connections", type=int, default=50)
    ap.add_argument("--sweep-connections", help="comma-separated connection counts")
    ap.add_argument("--wiremock-jar", default=DEFAULT_JAR)
    ap.add_argument("--wiremock-version", default=DEFAULT_WIREMOCK_VERSION)
    ap.add_argument("--rift-version", default="local")
    ap.add_argument("--mb-version", default="2.9.1")
    ap.add_argument("--java-version", default=None,
                    help="override the JVM string recorded in the report (default: probed)")
    ap.add_argument("--container-threads", type=int, default=None, metavar="N",
                    help="pin WireMock's Jetty container pool for the headline series (default: "
                         "max(cpu_count, highest connection count), so the pool is never the "
                         "concurrency ceiling). Set it explicitly to produce a thread-sweep curve "
                         "by hand — there is deliberately no standing sweep axis, the suite is "
                         "already slow. The stock-defaults secondary series ignores this.")
    ap.add_argument("--rep", type=int, default=None,
                    help="tag this run's CSV as _repN so several reps can be aggregated")
    args = ap.parse_args()

    suffix = f"_rep{args.rep}" if args.rep else ""
    conn_list = parse_conn_list(args.sweep_connections) if args.sweep_connections else [args.connections]
    # The stock series is benched at --connections only. If that point is not in the swept set, it
    # measures ~6 minutes of a configuration no --report can render (report() reads a single
    # connection point), so fail at parse time rather than after the run.
    if args.run_all and args.sweep_connections and args.connections not in conn_list:
        ap.error(f"--connections {args.connections} is not in --sweep-connections "
                 f"{sorted(conn_list)}; the stock series would bench a point no report can read. "
                 f"Pass --connections with one of the swept values.")

    threads = None
    if args.run_all:
        threads = run_all(args.wiremock_jar, args.duration, args.warmup, conn_list,
                          suffix, args.wiremock_version, args.container_threads,
                          stock_conns=args.connections)
    if args.report:
        java_ver = args.java_version
        if java_ver is None:
            try:
                _major, java_ver = java_preflight()
            except SystemExit:
                java_ver = "unknown"
        # A standalone `--report` did not run the bench. Read what the run recorded rather than
        # re-deriving it from THIS invocation's flags, which would print a number the run never
        # used. An explicit --container-threads still wins; absent both, the report says so.
        if threads is None:
            threads = (args.container_threads if args.container_threads is not None
                       else recorded_container_threads(suffix))
        report(args.rift_version, args.mb_version, args.wiremock_version, java_ver,
               args.duration, args.connections, suffix, container_threads=threads)
    if not (args.run_all or args.report):
        ap.error("nothing to do: pass --run-all and/or --report")
