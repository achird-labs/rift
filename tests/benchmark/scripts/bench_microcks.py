#!/usr/bin/env python3
"""Microcks benchmark suite (issue #900) — the fourth engine in the published comparison.

`bench_direct.py` compares Rift against Mountebank on byte-identical imposter JSON; `bench_wiremock.py`
adds WireMock by *generating* its mappings from that same fixture. Microcks needs a third step,
because it is not stub-authored at all: it is **spec-driven**. You do not hand Microcks a stub, you
hand it an OpenAPI document and it derives operations and example responses from it. So this suite
generates an OpenAPI 3.0.2 artifact per imposter and imports it through Microcks' own artifact API.

That difference is the single most important thing to understand about this comparison, and it is
stated in the report rather than buried here: **for Microcks, "410 stubs" is not a thing that
exists.** The nearest honest analogue is "410 matchable request shapes", which in OpenAPI means
410 `path` x `verb` operations. That mapping is what `openapi_spec` implements, and the
scenarios where it is *not* an honest analogue are refused outright — see `UNTRANSLATABLE`.

Like the WireMock suite this is a **separate** module rather than a fourth engine inside
`bench_direct.py`: the rift-vs-mb default run is a stability contract (`DefaultRunUnchanged`) that
must stay byte-comparable with previously published numbers, and not touching `bench_direct.py`
makes that risk zero by construction. The one source of truth is still shared — this module
**imports** `IMPOSTERS`, `SCENARIOS` and `EXPECT_BODY` from `bench_direct`, so the workloads cannot
drift apart.

Three fidelity properties worth knowing, all verified rather than assumed:

- **Bodies are byte-identical to Rift's.** An OpenAPI example whose `value` is a JSON *string* is
  served back verbatim by Microcks, so emitting the fixture's response body as a raw string (rather
  than a parsed object Microcks would re-serialize, dropping `json.dumps`' spaces) means
  `bench_direct.verify_body` and `EXPECT_BODY` work here unchanged. `test_bench_microcks.py` pins
  this — a regression to parsed values would silently swap the assertion for a weaker one.
- **No Docker.** The measured process is a plain native JVM, exactly like WireMock's standalone jar,
  because a container's network stack is not a property of the engine (see `MICROCKS_JAR_HELP` for
  where the jar comes from and why).
- **One imposter per process.** WireMock gets a fresh JVM per imposter, so its resident corpus is
  only ever that imposter's stubs. Microcks multiplexes every service on one port, so loading all
  14 imposters into one process would leave it holding ~1500 operations while WireMock holds 310 —
  and Microcks' throughput is *measurably* sensitive to total resident corpus size. This suite
  therefore runs one Microcks per imposter, sequentially, holding exactly that imposter's service.

Run:
    python3 bench_microcks.py --run-all      # bench Microcks alone -> direct_microcks.csv
    python3 bench_microcks.py --report       # combine whatever CSVs exist -> comparison report

Prerequisites: Temurin/OpenJDK **21** and the Microcks uber app jar (see `MICROCKS_JAR_HELP`).
"""
import argparse
import glob
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

sys.path.insert(0, os.path.dirname(__file__))
from bench_direct import (  # noqa: E402
    CSV_HEADER, IMPOSTERS, REP_FILE_RX, RESULTS_DIR, SCENARIOS,
    aggregate_reps, csv_row, free_ports, launch, load_rift_csv, metric, mode_label,
    parse_conn_list, run_oha, stop, verify_body, write_results_csv,
)

# rift = offset 0, mb = +100 (bench_direct), wiremock = +200, microcks = +300. Disjoint ranges are
# what let the engines run one at a time without ever being measured in place of each other.
MICROCKS_OFFSET = 300

# Microcks needs Java 21: the 1.14.0 app jar is Spring Boot 3.5.6 with class file version 65, so a
# Java 17 JRE fails at `JarLauncher.main` with `UnsupportedClassVersionError` and nothing earlier
# would explain why. `benchmark-publish.yml` already provisions Temurin 21 for the WireMock leg.
MIN_JAVA_MAJOR = 21

DEFAULT_JAR = os.path.expanduser("~/bench-microcks/microcks-app.jar")
DEFAULT_MICROCKS_VERSION = "1.14.0"

# Every service is imported under one version so the mock URL is derivable from the imposter name
# alone: /rest/<service>/<SERVICE_VERSION><path>.
SERVICE_VERSION = "1.0.0"

# Same value and same reason as the WireMock leg: a JIT needs more than 3s, and the published table
# is only like-for-like if every engine was measured at one warmup (issue #866).
DEFAULT_WARMUP = "10s"

# JVM heap. Fixed rather than left to the default ergonomic sizing, because the default is a
# fraction of *host* RAM and would therefore differ between a 16-vCPU runner and a laptop, silently
# making two runs non-comparable. 4g committed up front so heap growth is not measured as latency.
DEFAULT_HEAP = "4g"

MICROCKS_JAR_HELP = """\
Microcks publishes only non-repackaged jars to Maven Central (`microcks-app-1.14.0.jar` is 750KB of
classes with no `Main-Class`), so the runnable Spring Boot fat jar has to come out of the official
container image. That is a *download*, not a deployment — the benchmarked process is a native JVM:

  mkdir -p ~/bench-microcks
  cid=$(docker create microcks/microcks-uber:{version})
  docker cp "$cid:/deployments/app.jar" ~/bench-microcks/microcks-app.jar
  docker rm -f "$cid"

`docker create` does not start the container; it only materialises its filesystem. If you would
rather not involve a registry at all, build `microcks-app` from source at tag {version} — the jar
this suite wants is the Spring Boot repackaged one whose manifest says
`Start-Class: io.github.microcks.MicrocksApplication`."""


# --------------------------------------------------------------------------------------------
# What can and cannot be translated
# --------------------------------------------------------------------------------------------
#
# The issue behind this suite (#900) says the fairness section will be read adversarially by people
# who like Microcks, and it should be. The failure mode that would deserve that criticism is not
# omitting a scenario — it is *including* one under a translation that quietly measures something
# else, then publishing the ratio. So the comparable set is an explicit allow-list, every exclusion
# carries its reason, and the reasons are printed in the report instead of living in a comment.

# Scenarios translated faithfully: path/verb dispatch (the family the stub-growth claim rests on)
# plus query-argument dispatch.
COMPARABLE = ("simple_health", "api_first", "api_middle", "api_last", "no_match", "query_last")

UNTRANSLATABLE = {
    "regex_last":
        "Rift matches the path against 100 regexes (`matches`). OpenAPI paths are templated, not "
        "regular expressions, and Microcks has no regex path dispatcher — `/regex/pattern{n}/{x}` "
        "as a path template is a different matcher doing less work, so any number would flatter "
        "Microcks without being comparable.",
    "complex_predicate":
        "Rift evaluates `and`/`or` over method, path prefix and two alternative headers. Microcks "
        "has no header dispatcher for REST; the only way to express it is a Groovy SCRIPT "
        "dispatcher, which measures the scripting engine rather than a matcher.",
    "header_last":
        "100 stubs discriminated solely by an `X-Route-Id` header. Same reason as "
        "`complex_predicate`: no header dispatcher for REST.",
    "json_body_equals":
        "Rift matches method + path + an exact JSON body. The 50 stubs sit on 50 *distinct* paths, "
        "so a faithful-looking Microcks translation (50 operations, one response each) would "
        "resolve on the path alone and never inspect the body — strictly less work than Rift did.",
    "jsonpath":
        "Rift applies a JSONPath selector (`$.user.id`) as a predicate modifier. Microcks' "
        "JSON_BODY dispatcher uses its own expression dialect with different semantics; equating "
        "the two would be an assertion about dialects, not about matching cost.",
    "xpath":
        "Rift applies an XPath selector to an XML body. Microcks has no XPath dispatcher for REST.",
    "template":
        "Rift interpolates `${request.path}` / `${request.query}` into the response. Microcks does "
        "have response templating, but with a different expression language, and the fixture's body "
        "marker is satisfied by a static example — so including it would report a static-response "
        "number under a templating label.",
}

# `no_match` is comparable but not identical, and the difference is a status code rather than a
# translation choice. Rift and Mountebank answer an unmatched request with an empty 200; WireMock
# 404s, and the WireMock suite reproduces the Rift default by installing a catch-all empty-200
# mapping. Microcks has no catch-all mechanism — you cannot register a fallback for an unknown path
# — so it is measured as the 404 it genuinely returns, and the status gate below expects that.
# The body is empty either way, so `verify_body`'s `EXPECT_BODY[no_match] is None` check still holds
# and still proves nothing matched.
EXPECT_STATUS_PREFIX = {"no_match": "4"}


def comparable_scenarios():
    """The SCENARIOS rows this suite measures, in fixture order.

    Derived from `SCENARIOS` rather than restated, so a fixture change cannot leave this suite
    benching a scenario definition that no longer exists."""
    return [s for s in SCENARIOS if s[0] in COMPARABLE]


def check_scenario_coverage():
    """Every fixture scenario must be either measured or explicitly refused.

    This is the guard that keeps the exclusion list honest as the fixture grows: a new scenario
    added to `bench_direct.SCENARIOS` fails here until someone decides whether Microcks can express
    it, instead of silently vanishing from a table that claims to cover the suite."""
    names = [s[0] for s in SCENARIOS]
    classified = set(COMPARABLE) | set(UNTRANSLATABLE)
    missing = [n for n in names if n not in classified]
    if missing:
        raise SystemExit(
            f"bench_microcks: scenario(s) {missing} are neither in COMPARABLE nor explained in "
            f"UNTRANSLATABLE. Decide which, with a reason — an unclassified scenario would drop out "
            f"of a report that claims to cover the whole suite.")
    unknown = [n for n in classified if n not in names]
    if unknown:
        raise SystemExit(
            f"bench_microcks: {unknown} classified but absent from bench_direct.SCENARIOS — the "
            f"fixture moved and this list did not.")


# --------------------------------------------------------------------------------------------
# Imposter -> OpenAPI translator
# --------------------------------------------------------------------------------------------

def _fail(msg):
    """Abort translation.

    Deliberately fatal rather than "skip the stub I don't understand", for the same reason as the
    WireMock translator: a silently dropped stub still passes the measured scenario's body
    assertion, and corrupts only the surrounding stubs — the ones whose whole job is to make the
    match work harder. That is exactly the mistake that publishes an inflated number."""
    raise SystemExit(f"bench_microcks: cannot translate stub — {msg}")


def _raw_body(resp):
    """The response body exactly as Rift would emit it, as a string.

    Returned as a *string* on purpose. Microcks serves a string example verbatim but re-serializes a
    parsed object, and `json.dumps` in the fixture emits `{"id": 1}` while Microcks' serializer
    emits `{"id":1}` — which would break every `EXPECT_BODY` marker that contains a space after a
    colon, and would have to be replaced by a weaker whitespace-insensitive assertion. Keeping the
    bytes identical keeps the strong assertion."""
    body = resp.get("body", "")
    if body is None:
        return ""
    if isinstance(body, (dict, list)):
        # The fixture always pre-serializes with json.dumps, so this is a fixture change rather
        # than an input we should guess at: guessing a separator here is how the bodies drift.
        _fail(f"response body is a {type(body).__name__}, not a pre-serialized string — "
              f"the byte-identical-body property depends on the fixture's own json.dumps output")
    return body


def _content_type(resp):
    """Content type the example must be filed under.

    Microcks keys examples by media type, so this decides where the body lands in the spec. Read the
    stub's own header when it sets one; otherwise infer, because the fixture omits the header on
    plain-text stubs (`/health` -> `OK`) and filing those as JSON would make Microcks refuse the
    artifact."""
    for k, v in (resp.get("headers") or {}).items():
        if k.lower() == "content-type":
            return v.split(";")[0].strip()
    body = resp.get("body", "")
    if isinstance(body, str) and body[:1] in ("{", "["):
        return "application/json"
    if isinstance(body, str) and body.lstrip().startswith("<"):
        return "application/xml"
    return "text/plain"


def _equals_fields(stub):
    """(method, path, query) from a single-`equals` stub, or `_fail`.

    The comparable set is exactly the path/verb and query-argument family, so anything else reaching
    the translator is a bug in the allow-list rather than a stub to interpret creatively."""
    preds = stub.get("predicates") or []
    if len(preds) != 1:
        _fail(f"expected exactly 1 predicate, got {len(preds)}: {json.dumps(preds)[:160]}")
    pred = preds[0]
    if set(pred) != {"equals"}:
        _fail(f"only a bare `equals` predicate is translatable here, got operators {sorted(pred)}")
    eq = pred["equals"]
    unsupported = set(eq) - {"method", "path", "query"}
    if unsupported:
        _fail(f"`equals` on {sorted(unsupported)} has no faithful OpenAPI analogue "
              f"(see UNTRANSLATABLE for why these scenarios are excluded)")
    if "path" not in eq:
        _fail(f"stub has no path to key an operation on: {json.dumps(eq)[:160]}")
    return eq.get("method", "GET").upper(), eq["path"], eq.get("query") or {}


def _example_name(i):
    """Stable per-stub example name. Microcks pairs a request example with a response example by
    NAME, so the query-dispatch translation depends on both sides using this same key."""
    return f"ex{i}"


def openapi_spec(service, stubs):
    """One OpenAPI 3.0.2 document expressing `stubs` as Microcks operations.

    Two shapes come out of this, and which one Microcks picks is driven by the fixture rather than
    by us:

    - **Distinct literal paths** (Simple, API) -> one operation per `path` x `verb`, each with a
      single response example. Microcks assigns no dispatcher; resolution is path/verb only. This is
      the honest analogue of "N stubs competing for one request", and it is the shape the
      stub-growth claim is measured on.
    - **One shared path discriminated by query args** (Query) -> a single operation carrying N
      *named* examples, with the query values as request-parameter examples under the same names.
      Microcks infers `dispatcher=URI_PARAMS` with rules `page && size` from that, which is its
      native way to express what Rift expresses as N query-`equals` stubs.
    """
    paths = {}
    for i, stub in enumerate(stubs, 1):
        method, path, query = _equals_fields(stub)
        responses = stub.get("responses") or []
        if len(responses) != 1 or "is" not in responses[0]:
            _fail(f"expected exactly one `is` response, got {json.dumps(responses)[:160]}")
        resp = responses[0]["is"]
        status = str(resp.get("statusCode", 200))
        body = _raw_body(resp)
        ctype = _content_type(resp)
        ex_name = _example_name(i)

        op = paths.setdefault(path, {}).setdefault(method.lower(), {
            "operationId": _operation_id(method, path),
            "parameters": [],
            "responses": {},
        })

        # A 204 carries no body, so it gets no content block — an empty example under a media type
        # makes Microcks emit a zero-length body with a Content-Type, which is not what Rift does.
        entry = op["responses"].setdefault(status, {"description": "generated from imposter stub"})
        if body != "":
            entry.setdefault("content", {}).setdefault(ctype, {}).setdefault("examples", {})[
                ex_name] = {"value": body}
        elif status == "204":
            pass
        else:
            entry.setdefault("content", {}).setdefault(ctype, {}).setdefault("examples", {})[
                ex_name] = {"value": ""}

        for qname, qvalue in query.items():
            _add_query_param(op, qname, ex_name, str(qvalue))

    for path_item in paths.values():
        for op in path_item.values():
            if not op["parameters"]:
                del op["parameters"]

    return {
        "openapi": "3.0.2",
        "info": {
            "title": service,
            "version": SERVICE_VERSION,
            "description": (
                "Generated by tests/benchmark/scripts/bench_microcks.py from the shared imposter "
                "fixture in bench_direct.py. Hand edits will be overwritten."),
        },
        "paths": paths,
    }


def _operation_id(method, path):
    """A unique, spec-legal operationId. Microcks does not dispatch on it, but a duplicate makes
    some OpenAPI parsers reject the whole document, which would look like an import failure."""
    slug = re.sub(r"[^A-Za-z0-9]+", "_", path).strip("_")
    return f"{method.lower()}_{slug}" if slug else method.lower()


def _add_query_param(op, name, example_name, value):
    """Attach one query-parameter example, creating the parameter on first sight.

    `required: True` matters: Microcks derives its `URI_PARAMS` dispatcher rules from the *required*
    query parameters that carry examples, so an optional parameter would be left out of the rules
    and every request would fall on the same example regardless of `page` — the 100-way dispatch
    would collapse to a 1-way one and the scenario would measure nothing."""
    for param in op["parameters"]:
        if param["name"] == name:
            param["examples"][example_name] = {"value": value}
            return
    op["parameters"].append({
        "name": name, "in": "query", "required": True,
        "schema": {"type": "string"},
        "examples": {example_name: {"value": value}},
    })


def expected_operations(stubs):
    """How many `path` x `verb` operations `stubs` should produce.

    The import verification below compares this against what Microcks actually registered. Same
    role as the WireMock suite's mapping-count check: an import that drops operations leaves the
    measured scenario passing while the stubs that make matching *work* have quietly gone missing."""
    seen = set()
    for stub in stubs:
        method, path, _ = _equals_fields(stub)
        seen.add((method, path))
    return len(seen)


# --------------------------------------------------------------------------------------------
# Java preflight + process model
# --------------------------------------------------------------------------------------------

def parse_java_major(version_output):
    """Major version from `java -version` output, or None if unparseable."""
    m = re.search(r'version "(\d+)(?:\.(\d+))?', version_output)
    if not m:
        return None
    major = int(m.group(1))
    if major == 1 and m.group(2):
        return int(m.group(2))
    return major


def java_preflight(java="java"):
    """Fail once, actionably, rather than as an `UnsupportedClassVersionError` stack trace."""
    try:
        out = subprocess.run([java, "-version"], capture_output=True, text=True, timeout=30)
    except FileNotFoundError:
        raise SystemExit(
            f"bench_microcks: `{java}` not found. Microcks 1.14 is Spring Boot 3.5 and needs "
            f"Java {MIN_JAVA_MAJOR}+ (Temurin 21 is what CI uses).")
    text = (out.stderr or "") + (out.stdout or "")
    major = parse_java_major(text)
    if major is None:
        raise SystemExit(f"bench_microcks: could not parse `{java} -version`: {text.strip()[:200]}")
    if major < MIN_JAVA_MAJOR:
        raise SystemExit(
            f"bench_microcks: java {major} is too old — the Microcks 1.14 app jar is class file "
            f"version 65, so it needs Java {MIN_JAVA_MAJOR}+. Point --java at a Temurin 21 JDK.")
    return major, (text.strip().splitlines()[0] if text.strip() else f"java {major}")


def tomcat_thread_count(conn_list, override=None):
    """Threads for Microcks' embedded Tomcat connector.

    Exactly the same fairness argument as the WireMock leg's `--container-threads`, and it bites for
    the same reason: Tomcat's `server.tomcat.threads.max` defaults to **200** while the published
    comparison drives 256 connections, so the stock configuration would bound in-flight requests
    below the offered concurrency and the run would measure the pool rather than the engine.
    `max(cpu_count, max(connections))` guarantees the pool is never the bottleneck. Extra threads
    do not manufacture parallelism — the CPU budget is unchanged — they only remove an artificial
    queue, so a Rift win measured against this pin is unambiguous."""
    if override is not None:
        if override < 1:
            raise SystemExit(f"bench_microcks: --tomcat-threads must be >= 1, got {override}")
        return override
    return max(os.cpu_count() or 1, max(conn_list))


def microcks_cmd(jar, port, threads, heap=DEFAULT_HEAP, java="java"):
    """The JVM argv for one Microcks instance.

    Every flag here is either a fairness requirement or a determinism requirement:

    * `-Dspring.profiles.active=uber` selects the standalone profile: Keycloak off, embedded
      in-memory MongoDB. Passed as a system property rather than the `SPRING_PROFILES_ACTIVE`
      environment variable the official image uses, because `bench_direct.launch` deliberately takes
      no `env` — every engine in this harness is launched from argv alone, so what a run was
      configured with is visible in the process table and in the log header. Worth stating plainly
      in the report: a production Microcks talks to a real MongoDB over a socket, so the in-memory
      store makes this configuration *faster* than a real deployment. That direction flatters
      Microcks, which is the safe direction for a comparison published by Rift.
    * `-Dspring.aot.enabled=true` matches the official image's own JAVA_OPTIONS and uses the
      AOT-generated context. Faster startup; again, flatters Microcks.
    * `-Dasync-api.enabled=false` switches off the AsyncAPI/WebSocket minion puller. It is not part
      of the HTTP mocking path this suite measures, and leaving it on would charge Microcks for
      background work under load that has nothing to do with the comparison.
    * `-Xms == -Xmx` pins the heap so it is identical on a laptop and a 16-vCPU runner, and so heap
      growth is never measured as request latency.
    * The log level is dropped to WARN. Per issue #718's finding on the Rift side, a per-request log
      site turns a throughput benchmark into a measurement of the logging pipeline; the same trap
      exists here and is worth an explicit flag rather than a hope about defaults.
    """
    return [
        java,
        f"-Xms{heap}", f"-Xmx{heap}",
        "-Dspring.profiles.active=uber",
        "-Dspring.aot.enabled=true",
        "-Dasync-api.enabled=false",
        "-jar", jar,
        f"--server.port={port}",
        f"--server.tomcat.threads.max={threads}",
        "--logging.level.root=WARN",
        "--logging.level.io.github.microcks=WARN",
        "--spring.main.banner-mode=off",
    ]


def api_ready(port, tries=360):
    """Readiness is `GET /api/version/info` answering 200.

    A bare TCP accept would race the Spring context: Tomcat binds before the mock controller and
    the Mongo store are wired, and an artifact upload against that window fails in a way that looks
    like a translation bug."""
    url = f"http://localhost:{port}/api/version/info"
    for _ in range(tries):
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def probe_microcks_version(port, fallback):
    """Report the version the JVM actually answers with, not the one we think we downloaded.

    Best-effort: a reporting detail should not fail a whole run, so fall back to the pinned value."""
    try:
        with urllib.request.urlopen(f"http://localhost:{port}/api/version/info", timeout=5) as r:
            parsed = json.loads(r.read().decode("utf-8", "replace"))
        return parsed.get("versionId") or fallback
    except Exception:
        return fallback


def _multipart(field, filename, payload):
    """Minimal multipart/form-data body.

    Hand-rolled because the artifact API is the only multipart call in the whole benchmark harness
    and the rest of it deliberately depends on nothing outside the standard library."""
    boundary = f"----bench{uuid.uuid4().hex}"
    body = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="{field}"; filename="{filename}"\r\n'
        f"Content-Type: application/json\r\n\r\n"
    ).encode() + payload + f"\r\n--{boundary}--\r\n".encode()
    return boundary, body


def upload_artifact(port, service, spec):
    """Import one generated OpenAPI document as a Microcks main artifact."""
    payload = json.dumps(spec, separators=(",", ":")).encode()
    boundary, body = _multipart("file", f"{service}-openapi.json", payload)
    req = urllib.request.Request(
        f"http://localhost:{port}/api/artifact/upload?mainArtifact=true",
        data=body, method="POST",
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            return r.read().decode("utf-8", "replace").strip()
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:400]
        raise SystemExit(f"bench_microcks: artifact upload failed on {port} for {service}: "
                         f"HTTP {e.code}: {detail}")


def registered_operations(port, service):
    """Operations Microcks registered for `service`, as a list of "VERB /path" strings."""
    url = (f"http://localhost:{port}/api/services/search"
           f"?name={urllib.parse.quote(service)}")
    with urllib.request.urlopen(url, timeout=60) as r:
        found = json.loads(r.read().decode("utf-8", "replace"))
    for svc in found:
        if svc.get("name") == service and svc.get("version") == SERVICE_VERSION:
            return [op.get("name", "") for op in (svc.get("operations") or [])]
    raise SystemExit(f"bench_microcks: service {service}:{SERVICE_VERSION} not found on {port} "
                     f"after upload — the import did not register it")


def load_service(port, service, stubs):
    """Translate, import, and prove the import landed intact.

    The count check is the same guard as the WireMock suite's mapping-count assertion, and exists
    for the same reason: Microcks accepts an artifact and reports 201 even when its parser has
    quietly discarded operations it did not like, and the measured scenario would still pass."""
    spec = openapi_spec(service, stubs)
    t0 = time.time()
    upload_artifact(port, service, spec)
    ops = registered_operations(port, service)
    want = expected_operations(stubs)
    if len(ops) != want:
        raise SystemExit(
            f"bench_microcks: {service} registered {len(ops)} operations, expected {want} — the "
            f"import dropped some, and measuring a thinner corpus than Rift's would be bogus")
    print(f"    {service} port={port} stubs={len(stubs)} operations={len(ops)} "
          f"in {time.time() - t0:.2f}s")
    return spec, ops


def mock_url(port, service, path):
    """Microcks serves REST mocks under /rest/<service>/<version><path>.

    The service name is URL-encoded because Microcks keys on `info.title` verbatim, and an imposter
    name is not guaranteed to be path-safe forever."""
    return f"http://localhost:{port}/rest/{urllib.parse.quote(service)}/{SERVICE_VERSION}{path}"


# --------------------------------------------------------------------------------------------
# Bench loop
# --------------------------------------------------------------------------------------------

def imposter_by_port():
    return {base_port: (name, stubs) for base_port, name, stubs in IMPOSTERS}


def scenario_groups():
    """Comparable scenarios grouped by the imposter that serves them, in fixture order.

    Grouping is what makes the one-imposter-per-process model cheap: the four API scenarios share a
    single Microcks launch, so the run costs one JVM start per *imposter* rather than per scenario."""
    groups, order = {}, []
    for row in comparable_scenarios():
        base_port = row[1]
        if base_port not in groups:
            groups[base_port] = []
            order.append(base_port)
        groups[base_port].append(row)
    return [(bp, groups[bp]) for bp in order]


def _status_ok(name, codes):
    """Whether a scenario's status distribution is the one it is supposed to have.

    Everything is 2xx except `no_match`, which Microcks answers with a 404 it has no catch-all
    mechanism to avoid (see `EXPECT_STATUS_PREFIX`). Pinning the *expected* prefix per scenario,
    rather than relaxing the gate to "any status", keeps the check able to catch a genuinely
    mis-served stub."""
    if not codes:
        return False
    want = EXPECT_STATUS_PREFIX.get(name, "2")
    return all(str(c).startswith(want) for c in codes)


def bench_microcks(jar, logdir, duration, warmup, conn_list, threads, heap, java,
                   csv_suffix="", engine="microcks", microcks_version=DEFAULT_MICROCKS_VERSION):
    """Launch one Microcks per imposter in turn, bench that imposter's scenarios, tear it down.

    Sequential and one-service-at-a-time on purpose — see the module docstring. The cost is ~14s of
    JVM start per imposter; the benefit is that Microcks' resident corpus is exactly the imposter
    under test, which is the only way this is comparable with WireMock's per-imposter JVM."""
    os.makedirs(RESULTS_DIR, exist_ok=True)
    os.makedirs(logdir, exist_ok=True)
    check_scenario_coverage()
    by_port = imposter_by_port()
    mode = mode_label(None)
    rows = []
    version = microcks_version

    for base_port, scenarios in scenario_groups():
        service, stubs = by_port[base_port]
        port = base_port + MICROCKS_OFFSET
        log = os.path.join(logdir, f"microcks-{service}-{port}.log")
        free_ports([port])
        print(f"[{engine}] {service}: launching on {port} "
              f"({threads} tomcat threads, heap {heap})")
        proc = launch(microcks_cmd(jar, port, threads, heap, java), log)
        try:
            if not api_ready(port):
                raise SystemExit(f"bench_microcks: {service} on {port} never became ready "
                                 f"(see {log})")
            load_service(port, service, stubs)
            version = probe_microcks_version(port, microcks_version)
            for name, _bp, method, path, body, headers in scenarios:
                url = mock_url(port, service, path)
                # The same two gates the other suites use, and the reason a mistranslation cannot
                # be published as a fast number: the body marker proves the intended operation
                # served the request, and an unexpected status distribution aborts the run.
                verify_body(engine, name, method, url, body, headers)
                for conns in conn_list:
                    run_oha(url, method, body, headers, warmup, conns)
                    m = metric(run_oha(url, method, body, headers, duration, conns))
                    total = sum(m["codes"].values())
                    ok = _status_ok(name, m["codes"]) and total > 0
                    status = "ok" if ok else f"BAD codes={m['codes']}"
                    print(f"  {name:20s} c={conns:<4d} {m['rps']:>10.1f} rps  "
                          f"p50={m['p50_ms']}ms p99={m['p99_ms']}ms p999={m['p999_ms']}ms  {status}")
                    if not ok:
                        raise SystemExit(f"{engine}/{name}: unexpected status distribution "
                                         f"{m['codes']} — aborting")
                    rows.append((name, conns, mode, m))
        finally:
            stop(proc, [port])
            free_ports([port])

    path = write_results_csv(engine, csv_suffix, rows)
    print(f"[{engine}] wrote {path}")
    record_run_settings(csv_suffix, warmup, threads, heap, version)
    return path


def run_all(jar, duration, warmup, conn_list, csv_suffix="",
            microcks_version=DEFAULT_MICROCKS_VERSION, tomcat_threads=None,
            heap=DEFAULT_HEAP, java="java"):
    major, java_line = java_preflight(java)
    print(f"[microcks] java {major} ({java_line})")
    if not os.path.exists(jar):
        raise SystemExit(
            f"bench_microcks: Microcks app jar not found at {jar}.\n\n"
            + MICROCKS_JAR_HELP.format(version=microcks_version))
    threads = tomcat_thread_count(conn_list, tomcat_threads)
    logdir = os.path.join(RESULTS_DIR, "logs")
    return bench_microcks(jar, logdir, duration, warmup, conn_list, threads, heap, java,
                          csv_suffix, "microcks", microcks_version)


# --------------------------------------------------------------------------------------------
# Run-settings sidecars (same pattern as the WireMock leg)
# --------------------------------------------------------------------------------------------

def sidecar_path(name, csv_suffix):
    return os.path.join(RESULTS_DIR, f"direct_microcks{csv_suffix}.{name}")


def record_run_settings(csv_suffix, warmup, threads, heap, version):
    """Persist what the run was invoked with, beside its CSV.

    The report is generated in a separate process (`--report`, often a separate CI step), so a
    setting that only ever lived in argv cannot be stated in the published table — and an
    unstateable setting is indistinguishable from an unfair one to a reader."""
    for name, value in (("warmup", warmup), ("threads", threads), ("heap", heap),
                        ("version", version)):
        try:
            with open(sidecar_path(name, csv_suffix), "w") as fh:
                fh.write(str(value))
        except OSError:
            pass


def recorded_setting(name, csv_suffix, default=None):
    try:
        with open(sidecar_path(name, csv_suffix)) as fh:
            return fh.read().strip() or default
    except OSError:
        return default


def propagate_run_settings(base_suffix=""):
    """Copy per-rep sidecars up to the aggregated suffix, so `--report` after an aggregation can
    still state the settings the reps ran with."""
    for name in ("warmup", "threads", "heap", "version"):
        if recorded_setting(name, base_suffix) is not None:
            continue
        for rep_file in sorted(glob.glob(sidecar_path(name, f"{base_suffix}_rep*"))):
            try:
                with open(rep_file) as fh:
                    value = fh.read().strip()
                with open(sidecar_path(name, base_suffix), "w") as out:
                    out.write(value)
                break
            except OSError:
                pass


# --------------------------------------------------------------------------------------------
# Aggregation across repetitions
# --------------------------------------------------------------------------------------------

def find_rep_files(base_suffix=""):
    """Every `direct_microcks<base_suffix>_repN.csv`, in rep order.

    Matching on the shared `_rep<digits>.csv` tail (`REP_FILE_RX`) rather than the glob alone keeps
    a stale unsuffixed file out of the aggregate."""
    pattern = os.path.join(RESULTS_DIR, f"direct_microcks{base_suffix}_rep*.csv")
    matched = [(int(m.group(1)), p) for p in glob.glob(pattern)
               for m in [REP_FILE_RX.search(p)] if m]
    return [p for _n, p in sorted(matched)]


def aggregate(base_suffix=""):
    """Collapse this engine's reps into `direct_microcks<base_suffix>_median.csv`.

    The median maths is `bench_direct.aggregate_reps`, imported rather than reimplemented — issue
    #746's lesson was that a single rep can land on a degraded host, and a per-engine definition of
    "median" would reintroduce that asymmetry by the back door. The output filename and the two
    extra columns (`reps`, `rps_spread_pct`) match the WireMock leg so `--report --csv-suffix
    <base>_median` reads every engine the same way."""
    paths = find_rep_files(base_suffix)
    if not paths:
        return None
    reps = []
    for path in paths:
        with open(path) as fh:
            reps.append(load_rift_csv(fh))
    agg = aggregate_reps(reps)

    # Same hard error as the other suites (#773): a point missing from one rep yields a
    # complete-looking report whose cells rest on fewer samples than it claims, with exit 0 and
    # nothing said. Refuse rather than publish that.
    incomplete = {k: c["reps"] for k, c in agg.items() if c["reps"] != len(paths)}
    if incomplete:
        detail = ", ".join(f"{s}@c={c}/{m}: {n} of {len(paths)}"
                           for (s, c, m), n in sorted(incomplete.items())[:8])
        raise SystemExit(
            f"bench_microcks: incomplete repetitions across {len(paths)} rep files: {detail}"
            f"{' …' if len(incomplete) > 8 else ''}\n"
            f"Every point must appear in every rep, or the median silently rests on fewer samples "
            f"than the report claims. Re-run the missing reps, or aggregate a consistent subset.")

    out = os.path.join(RESULTS_DIR, f"direct_microcks{base_suffix}_median.csv")
    with open(out, "w") as fh:
        fh.write(CSV_HEADER + ",reps,rps_spread_pct\n")
        for (scen, conns, mode), c in sorted(agg.items(), key=lambda kv: (kv[0][1], kv[0][0])):
            spread = f"{c['rps_spread_pct']:.1f}" if c["rps_spread_pct"] != "" else ""
            fh.write(f"{csv_row(scen, conns, mode, c)},{c['reps']},{spread}\n")
    propagate_run_settings(base_suffix)
    print(f"[microcks] {len(paths)} reps -> {os.path.basename(out)}; "
          f"render with --report --csv-suffix {base_suffix}_median")
    return out, len(paths)


# --------------------------------------------------------------------------------------------
# Report
# --------------------------------------------------------------------------------------------

def engine_csv_path(engine, csv_suffix):
    return os.path.join(RESULTS_DIR, f"direct_{engine}{csv_suffix}.csv")


def load_engine_csv(engine, csv_suffix, conns):
    """{scenario: {rps, p50, p99, reps, spread}} for one engine at the comparison point, or {}.

    Filtering on mode+connections is the stale-artefact guard the WireMock leg uses for the same
    reason: a CSV left behind by a sweep or an open-loop run holds rows that are not comparable, and
    blending them into the headline table would be silently wrong."""
    path = engine_csv_path(engine, csv_suffix)
    if not os.path.exists(path):
        return {}
    with open(path) as fh:
        return {r["scenario"]: {"rps": r["rps"], "p50": r["p50_ms"], "p99": r["p99_ms"],
                                # Present only in a `_median.csv`. A median with no visible spread
                                # cannot distinguish a clean run from one where a rep disagreed.
                                "reps": r.get("reps", ""), "spread": r.get("rps_spread_pct", "")}
                for r in load_rift_csv(fh)
                if r.get("mode", "closed") == "closed" and int(r["connections"]) == conns}


def _fmt(value, digits=0):
    if value in (None, ""):
        return "—"
    try:
        return f"{float(value):,.{digits}f}"
    except (TypeError, ValueError):
        return str(value)


def _ms(value):
    """A latency cell. The unit goes inside, so an absent value renders as a bare em dash rather
    than the nonsense `— ms`."""
    formatted = _fmt(value, 2)
    return formatted if formatted == "—" else f"{formatted} ms"


def _ratio(rift_rps, other_rps):
    try:
        r, o = float(rift_rps), float(other_rps)
        return f"**{r / o:.1f}x**" if o > 0 else "—"
    except (TypeError, ValueError):
        return "—"


def report(conns, csv_suffix="", duration=None):
    """Write the Microcks comparison report.

    A report of its own rather than a fourth column bolted onto the WireMock one: the comparable
    scenario set is a strict subset (six of thirteen), so the two tables have different row sets and
    merging them would either drop WireMock rows or show empty Microcks cells that read as "slow"
    rather than "not comparable"."""
    propagate_run_settings(csv_suffix)
    rift = load_engine_csv("rift", csv_suffix, conns)
    microcks = load_engine_csv("microcks", csv_suffix, conns)
    wiremock = load_engine_csv("wiremock", csv_suffix, conns)
    if not microcks:
        raise SystemExit(f"bench_microcks: no Microcks CSV at {engine_csv_path('microcks', csv_suffix)} "
                         f"— run `--run-all` first")

    version = recorded_setting("version", csv_suffix, DEFAULT_MICROCKS_VERSION)
    threads = recorded_setting("threads", csv_suffix, "?")
    heap = recorded_setting("heap", csv_suffix, DEFAULT_HEAP)
    warmup = recorded_setting("warmup", csv_suffix, DEFAULT_WARMUP)

    lines = [
        "# Rift vs Microcks — HTTP mock serving",
        "",
        f"Microcks **{version}** (native JVM, `uber` profile), "
        f"Tomcat max threads **{threads}**, heap **{heap}**, warmup **{warmup}**, "
        f"**{conns}** keep-alive connections"
        + (f", **{duration}** per scenario point." if duration else "."),
        "",
        "Generated by `tests/benchmark/scripts/bench_microcks.py`. Every engine ran alone, on a "
        "disjoint port range, one imposter per process.",
        "",
        "## Throughput and latency",
        "",
        "| Scenario | Microcks | Rift | Rift/Microcks | Microcks p50 | Rift p50 | Microcks p99 | Rift p99 |",
        "|:---------|---------:|-----:|--------------:|-------------:|---------:|-------------:|---------:|",
    ]
    for name in COMPARABLE:
        mrow = microcks.get(name)
        if not mrow:
            continue
        rrow = rift.get(name) or {}
        lines.append(
            f"| {name} | {_fmt(mrow.get('rps'))} | {_fmt(rrow.get('rps'))} | "
            f"{_ratio(rrow.get('rps'), mrow.get('rps'))} | "
            f"{_ms(mrow.get('p50'))} | {_ms(rrow.get('p50'))} | "
            f"{_ms(mrow.get('p99'))} | {_ms(rrow.get('p99'))} |")

    spreads = [f"{e} ≤{max(float(r['spread']) for r in d.values() if r.get('spread')):.1f}%"
               for e, d in (("Microcks", microcks), ("Rift", rift), ("WireMock", wiremock))
               if d and any(r.get("spread") for r in d.values())]
    if spreads:
        lines += ["", f"<sub>Median of repetitions; peak-to-peak spread: {', '.join(spreads)}.</sub>"]

    lines += ["", "## Stub growth", "",
              "Read each engine down its own column, not across. The question this suite exists to "
              "answer is whether throughput holds as the number of matchable request shapes grows, "
              "which is the interval between `simple_health` and `api_last`.", ""]
    growth = [("simple_health", "trivial stub"), ("api_first", "first of the API corpus"),
              ("api_middle", "middle of the API corpus"), ("api_last", "last of the API corpus")]
    lines += ["| Point | Workload | Microcks | Rift | WireMock |",
              "|:------|:---------|---------:|-----:|---------:|"]
    for name, label in growth:
        lines.append(f"| {name} | {label} | {_fmt(microcks.get(name, {}).get('rps'))} | "
                     f"{_fmt(rift.get(name, {}).get('rps'))} | "
                     f"{_fmt(wiremock.get(name, {}).get('rps'))} |")
    for engine_name, data in (("Microcks", microcks), ("Rift", rift), ("WireMock", wiremock)):
        lo, hi = data.get("simple_health", {}).get("rps"), data.get("api_last", {}).get("rps")
        if lo and hi:
            try:
                delta = (float(hi) - float(lo)) / float(lo) * 100.0
                lines.append("")
                lines.append(f"- **{engine_name}:** {_fmt(lo)} -> {_fmt(hi)} RPS "
                             f"({delta:+.0f}%) from a trivial stub to the last of the API corpus.")
            except (TypeError, ValueError, ZeroDivisionError):
                pass

    lines += ["", "## What is not comparable, and why", "",
              "Microcks is spec-driven, so several fixture scenarios have no faithful OpenAPI "
              "analogue. They are **excluded rather than approximated** — an approximation would "
              "publish a ratio between two different workloads:", ""]
    for name in sorted(UNTRANSLATABLE):
        lines.append(f"- **`{name}`** — {UNTRANSLATABLE[name]}")

    lines += ["", "## Where this comparison is not apples-to-apples", "",
              "- **Stubs vs specification.** Rift and WireMock are handed stubs; Microcks is handed "
              "an OpenAPI document and derives operations from it. \"N stubs\" is translated as "
              "\"N `path` x `verb` operations\", which is the nearest honest analogue, but it is a "
              "translation and not an identity.",
              "- **No catch-all for `no_match`.** Rift answers an unmatched request with an empty "
              "200 and WireMock is given a catch-all mapping to reproduce that. Microcks has no "
              "catch-all mechanism, so it is measured as the 404 it genuinely returns — a cheaper "
              "path than rendering a response, so this row flatters Microcks.",
              "- **In-memory MongoDB.** The `uber` profile stores everything in an embedded "
              "in-memory Mongo. A production Microcks talks to a real MongoDB over a socket, so "
              "this configuration is faster than a real deployment — again, flattering Microcks.",
              "- **AsyncAPI disabled.** `-Dasync-api.enabled=false` removes background work "
              "unrelated to HTTP mocking. Flatters Microcks.",
              "- **One service per process.** Microcks multiplexes every service on one port, and "
              "its throughput is sensitive to total resident corpus size, so loading all 14 "
              "imposters into one JVM would have penalised it against WireMock's per-imposter JVM. "
              "Each Microcks instance here holds exactly the imposter under test.",
              "- **The first scenario in each group meets a colder JIT.** Scenarios sharing an "
              "imposter share one JVM and run in fixture order, so `api_first` is measured on a "
              "younger JIT than `api_last`. The per-scenario warmup exists to absorb this and the "
              "median-of-reps limits what survives it, but at short warmups it is visible — and it "
              "biases *against* whichever scenario runs first, not against Microcks as a whole. "
              "WireMock's leg has the same property, so the two are consistent.",
              "- **Protocol scope.** Microcks also covers AsyncAPI, Kafka, MQTT, AMQP, WebSocket "
              "and gRPC, which Rift does not. This benchmark is deliberately confined to the HTTP "
              "matching path where the two overlap, and says nothing about the rest.",
              ""]

    os.makedirs(RESULTS_DIR, exist_ok=True)
    out = os.path.join(RESULTS_DIR, "MICROCKS_BENCHMARK_REPORT.md")
    with open(out, "w") as fh:
        fh.write("\n".join(lines) + "\n")
    print(f"[microcks] wrote {out}")
    return out


# --------------------------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------------------------

def resolve_suffix(csv_suffix, rep):
    return f"{csv_suffix}_rep{rep}" if rep else csv_suffix


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description="Microcks benchmark suite (issue #900)")
    ap.add_argument("--run-all", action="store_true", help="bench Microcks and write its CSV")
    ap.add_argument("--report", action="store_true", help="write the comparison report from CSVs")
    ap.add_argument("--aggregate", action="store_true", help="median-of-reps into the base CSV")
    ap.add_argument("--emit-spec", metavar="IMPOSTER",
                    help="print the generated OpenAPI document for one imposter and exit")
    ap.add_argument("--duration", default="20s")
    ap.add_argument("--warmup", default=DEFAULT_WARMUP)
    ap.add_argument("--connections", type=int, default=50)
    ap.add_argument("--sweep-connections", default=None,
                    help="comma-separated connection counts, e.g. 50,256")
    ap.add_argument("--jar", default=DEFAULT_JAR)
    ap.add_argument("--java", default="java", help="JDK 21+ launcher to run Microcks with")
    ap.add_argument("--heap", default=DEFAULT_HEAP)
    ap.add_argument("--tomcat-threads", type=int, default=None)
    ap.add_argument("--microcks-version", default=DEFAULT_MICROCKS_VERSION)
    ap.add_argument("--csv-suffix", default="")
    ap.add_argument("--rep", type=int, default=None)
    args = ap.parse_args()

    if args.emit_spec:
        by_name = {name: stubs for _, name, stubs in IMPOSTERS}
        if args.emit_spec not in by_name:
            raise SystemExit(f"bench_microcks: unknown imposter {args.emit_spec!r}; "
                             f"choose from {', '.join(sorted(by_name))}")
        print(json.dumps(openapi_spec(args.emit_spec, by_name[args.emit_spec]), indent=2))
        sys.exit(0)

    conn_list = (parse_conn_list(args.sweep_connections) if args.sweep_connections
                 else [args.connections])
    suffix = resolve_suffix(args.csv_suffix, args.rep)

    if args.run_all:
        run_all(args.jar, args.duration, args.warmup, conn_list, suffix,
                args.microcks_version, args.tomcat_threads, args.heap, args.java)
    if args.aggregate:
        aggregate(args.csv_suffix)
    if args.report:
        report(conn_list[-1], args.csv_suffix, args.duration)
    if not (args.run_all or args.report or args.aggregate):
        ap.print_help()
