#!/usr/bin/env python3
"""Gate tests for the WireMock suite's pure logic (issue #861).

The live benchmark path (14 JVMs + oha) is not CI-runnable, so these tests pin what actually
decides whether a published number is honest: the stub translation. A wrong translation does not
crash — it falls through to WireMock's no-match default, which is precisely why the translator
fails loudly on anything it does not recognise and why the completeness test below runs the *live*
imported fixture through it.

Run: python3 -m unittest test_bench_wiremock   (from tests/benchmark/scripts)
"""
import copy
import csv
import glob
import json
import os
import re
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import bench_direct as bd  # noqa: E402
import bench_wiremock as bw  # noqa: E402


def _translated(stubs):
    """Mappings without the trailing catch-all."""
    return bw.wiremock_mappings(stubs)[:-1]


class GoldenMappings(unittest.TestCase):
    """One representative stub from each generator -> the exact mapping expected."""

    def test_api_stub_equals_method_and_path(self):
        m = _translated(bd.api_stubs(resources=1, per=0))
        self.assertEqual(m[0], {
            "priority": 1,
            "request": {"method": "GET", "urlPath": "/api/v1/resource1"},
            "response": {"status": 200, "headers": {"Content-Type": "application/json"},
                         "body": json.dumps({"items": [{"id": 1}, {"id": 2}], "total": 2})},
        })

    def test_status_only_response_has_no_body_key(self):
        # api_stubs' DELETE is a 204 with no body; emitting `"body": ""` would be a different
        # response than the other engines send.
        m = _translated(bd.api_stubs(resources=1, per=1))
        delete = [x for x in m if x["request"].get("method") == "DELETE"][0]
        self.assertEqual(delete["response"], {"status": 204})

    def test_regex_stub_uses_urlPathPattern(self):
        m = _translated(bd.regex_stubs(n=1))
        self.assertEqual(m[0]["request"], {"urlPathPattern": "/regex/pattern1/[a-zA-Z0-9]+"})

    def test_complex_stub_splits_the_or_into_two_mappings(self):
        m = _translated(bd.complex_stubs(n=1))
        self.assertEqual(len(m), 2, "an OR over two different headers cannot be one mapping")
        for mapping in m:
            self.assertEqual(mapping["request"]["method"], "POST")
            self.assertEqual(mapping["request"]["urlPathPattern"], r"/complex/1/.*")
        self.assertEqual(m[0]["request"]["headers"], {"X-Request-Type": {"contains": "json"}})
        self.assertEqual(m[1]["request"]["headers"], {"Content-Type": {"contains": "application/json"}})
        # Same response on both branches, and consecutive priorities so neither can outrank the
        # other by WireMock's most-recently-added tiebreak.
        self.assertEqual(m[0]["response"], m[1]["response"])
        self.assertEqual([m[0]["priority"], m[1]["priority"]], [1, 2])

    def test_json_body_stub_is_tolerant_equalToJson(self):
        m = _translated(bd.json_body_stubs(n=1))
        self.assertEqual(m[0]["request"], {
            "method": "POST", "urlPath": "/json/equals/1",
            "bodyPatterns": [{"equalToJson": '{"id":1,"type":"request"}',
                              "ignoreExtraElements": True}],
        })

    def test_deepequals_body_stub_is_strict(self):
        m = _translated(bd.deepequals_body_stubs(n=1))
        pattern = m[0]["request"]["bodyPatterns"][0]
        self.assertIn("equalToJson", pattern)
        self.assertNotIn("ignoreExtraElements", pattern,
                         "deepEquals must not tolerate extra fields — that is what makes it deep")

    def test_jsonpath_stub_becomes_matchesJsonPath(self):
        m = _translated(bd.jsonpath_stubs(n=1))
        self.assertEqual(m[0]["request"], {
            "method": "POST", "urlPath": "/jsonpath/1",
            "bodyPatterns": [{"matchesJsonPath": {"expression": "$.user.id", "equalTo": "1"}}],
        })

    def test_xpath_stub_becomes_matchesXPath(self):
        m = _translated(bd.xpath_stubs(n=1))
        self.assertEqual(m[0]["request"], {
            "method": "POST", "urlPath": "/xpath/1",
            "bodyPatterns": [{"matchesXPath": "//item[@id='1']"}],
        })

    def test_template_stub_body_is_served_literally(self):
        # Response templating stays disabled, so `${request.path}` must survive into the mapping
        # verbatim — Mountebank serves it literally too, and EXPECT_BODY was chosen to match that.
        m = _translated(bd.template_stubs(n=1))
        self.assertIn("${request.path}", m[0]["response"]["body"])
        self.assertEqual(m[0]["response"]["headers"]["X-Request-Path"], "${request.path}")

    def test_header_stub_uses_equalTo_header_matcher(self):
        m = _translated(bd.header_stubs(n=1))
        self.assertEqual(m[0]["request"], {
            "urlPath": "/headers/route", "headers": {"X-Route-Id": {"equalTo": "route-1"}},
        })

    def test_query_stub_keeps_urlPath_and_adds_queryParameters(self):
        m = _translated(bd.query_stubs(n=1))
        self.assertEqual(m[0]["request"], {
            "urlPath": "/query/search",
            "queryParameters": {"page": {"equalTo": "1"}, "size": {"equalTo": "10"}},
        })

    def test_simple_stubs(self):
        m = _translated(bd.simple_stubs())
        self.assertEqual(m[0]["request"], {"urlPath": "/health"})
        self.assertEqual(m[0]["response"], {"status": 200, "body": "OK"})

    def test_literal_prefix_and_contains_become_patterns(self):
        m = _translated(bd.literal_prefix_stubs(n=1))
        self.assertEqual(m[0]["request"]["urlPathPattern"], r"/literal/prefix1/.*")
        self.assertEqual(m[1]["request"]["urlPathPattern"], r".*/needle1/.*")

    def test_literal_matchers_escape_regex_metacharacters(self):
        # `startsWith`/`contains` are LITERAL in Mountebank but land in a WireMock *regex*
        # (`urlPathPattern`), so the literal must be escaped on the way in. Every path in the
        # current fixture is metacharacter-free, which means the fixture alone cannot tell an
        # escaping translator from a non-escaping one — this test can. Without it, a future stub
        # path containing `.` or `+` would silently match the wrong requests.
        starts = _translated([{"predicates": [{"startsWith": {"path": "/v1.0/items+/"}}],
                               "responses": [{"is": {"statusCode": 200}}]}])
        self.assertEqual(starts[0]["request"]["urlPathPattern"], re.escape("/v1.0/items+/") + ".*")
        self.assertIn(r"\.", starts[0]["request"]["urlPathPattern"])
        self.assertIn(r"\+", starts[0]["request"]["urlPathPattern"])

        contains = _translated([{"predicates": [{"contains": {"path": "/a.b+c/"}}],
                                 "responses": [{"is": {"statusCode": 200}}]}])
        self.assertEqual(contains[0]["request"]["urlPathPattern"],
                         ".*" + re.escape("/a.b+c/") + ".*")

    def test_method_mix_stub_keeps_the_verb(self):
        m = _translated(bd.method_mix_stubs(n=1))
        self.assertEqual({x["request"]["method"] for x in m},
                         {"PUT", "DELETE", "PATCH", "OPTIONS"})

    def test_body_field_stubs_share_a_path_and_differ_only_by_body(self):
        m = _translated(bd.body_field_stubs(n=2))
        self.assertEqual({x["request"]["urlPath"] for x in m}, {"/orders/submit"})
        self.assertNotEqual(m[0]["request"]["bodyPatterns"], m[1]["request"]["bodyPatterns"])


class Ordering(unittest.TestCase):
    """Rift/MB take the first matching stub; WireMock takes the lowest priority, breaking ties by
    most-recently-added — so a duplicate priority silently INVERTS the intended order."""

    def test_priorities_are_strictly_increasing_across_an_or_split(self):
        stubs = bd.simple_stubs() + bd.complex_stubs(n=2) + bd.simple_stubs()
        mappings = _translated(stubs)
        priorities = [m["priority"] for m in mappings]
        self.assertEqual(priorities, sorted(priorities))
        self.assertEqual(len(priorities), len(set(priorities)), "duplicate priority inverts order")

    def test_priority_matches_stub_index_when_nothing_splits(self):
        mappings = _translated(bd.regex_stubs(n=5))
        self.assertEqual([m["priority"] for m in mappings], [1, 2, 3, 4, 5])

    def test_every_translated_mapping_outranks_the_catch_all(self):
        mappings = bw.wiremock_mappings(bd.api_stubs())
        self.assertEqual(mappings[-1]["priority"], bw.CATCH_ALL_PRIORITY)
        for m in mappings[:-1]:
            self.assertLess(m["priority"], bw.CATCH_ALL_PRIORITY)


class CatchAll(unittest.TestCase):
    """Rift/MB answer an unmatched request with an empty 200; WireMock answers 404 with diagnostic
    text, which would fail `no_match`'s empty-body assertion and trip the all-2xx gate."""

    def test_catch_all_is_last_and_returns_empty_200(self):
        for _port, name, stubs in bd.IMPOSTERS:
            with self.subTest(imposter=name):
                last = bw.wiremock_mappings(stubs)[-1]
                self.assertEqual(last, {
                    "priority": bw.CATCH_ALL_PRIORITY,
                    "request": {"urlPattern": ".*"},
                    "response": {"status": 200, "body": ""},
                })


class TranslatorCompleteness(unittest.TestCase):
    """Runs EVERY stub in the live imported fixture through the translator.

    This is the anti-drift guard: because the suite imports `IMPOSTERS` rather than copying it, a
    stub generator added to `bench_direct.py` later is seen here automatically and fails loudly if
    it uses an operator the translator does not implement."""

    def test_every_fixture_stub_translates(self):
        for _port, name, stubs in bd.IMPOSTERS:
            with self.subTest(imposter=name):
                mappings = bw.wiremock_mappings(stubs)
                self.assertGreaterEqual(len(mappings), len(stubs) + 1)
                for m in mappings:
                    self.assertIn("request", m)
                    self.assertIn("response", m)

    def test_every_comparison_scenario_has_an_imposter_and_a_marker(self):
        ports = {p for p, _, _ in bd.IMPOSTERS}
        for name, port, _method, _path, _body, _headers in bd.SCENARIOS:
            with self.subTest(scenario=name):
                self.assertIn(port, ports)
                self.assertIn(name, bd.EXPECT_BODY)


class UnknownOperator(unittest.TestCase):
    """A silently skipped stub still passes most scenarios and corrupts the one it mattered for."""

    def _expect_fail(self, stub, needle):
        with self.assertRaises(SystemExit) as ctx:
            bw.wiremock_mappings([stub])
        self.assertIn(needle, str(ctx.exception))

    def test_unknown_operator_raises(self):
        self._expect_fail({"predicates": [{"endsWith": {"path": "/x"}}],
                           "responses": [{"is": {"statusCode": 200}}]}, "endsWith")

    def test_unknown_field_raises(self):
        self._expect_fail({"predicates": [{"equals": {"protocol": "http"}}],
                           "responses": [{"is": {"statusCode": 200}}]}, "protocol")

    def test_non_is_response_raises(self):
        self._expect_fail({"predicates": [{"equals": {"path": "/x"}}],
                           "responses": [{"proxy": {"to": "http://x"}}]}, "proxy")

    def test_unsupported_is_field_raises(self):
        self._expect_fail({"predicates": [{"equals": {"path": "/x"}}],
                           "responses": [{"is": {"statusCode": 200, "_behaviors": {"wait": 1}}}]},
                          "_behaviors")

    def test_multiple_responses_raise(self):
        self._expect_fail({"predicates": [{"equals": {"path": "/x"}}],
                           "responses": [{"is": {"statusCode": 200}}, {"is": {"statusCode": 201}}]},
                          "exactly one response")

    def test_unanchored_matches_pattern_raises(self):
        # MB `matches` is an unanchored search; urlPathPattern anchors. Translating an unanchored
        # pattern as anchored would change which requests match.
        self._expect_fail({"predicates": [{"matches": {"path": "pattern[0-9]+"}}],
                           "responses": [{"is": {"statusCode": 200}}]}, "not anchored")

    def test_deepequals_headers_and_query_raise(self):
        # WireMock's headers/queryParameters are SUBSET matchers with no "and nothing else", so a
        # deepEquals over them has no faithful translation. Translating it as a subset anyway would
        # be worse than failing: it is also strictly cheaper than the exact-set check Rift/MB run,
        # so it would quietly flatter WireMock in the published table.
        for field, value in (("headers", {"A": "b"}), ("query", {"page": "1"})):
            with self.subTest(field=field):
                self._expect_fail(
                    {"predicates": [{"deepEquals": {"path": "/x", field: value}}],
                     "responses": [{"is": {"statusCode": 200}}]},
                    f"deepEquals.{field}")

    def test_equals_headers_and_query_still_translate(self):
        # The guard must be scoped to deepEquals — plain `equals` IS a subset match in MB, so
        # queryParameters/headers are the faithful translation there.
        m = _translated([{"predicates": [{"equals": {"path": "/x", "headers": {"A": "b"},
                                                     "query": {"page": "1"}}}],
                          "responses": [{"is": {"statusCode": 200}}]}])
        self.assertEqual(m[0]["request"]["headers"], {"A": {"equalTo": "b"}})
        self.assertEqual(m[0]["request"]["queryParameters"], {"page": {"equalTo": "1"}})

    def test_two_url_matchers_raise(self):
        self._expect_fail({"predicates": [{"and": [{"equals": {"path": "/a"}},
                                                   {"startsWith": {"path": "/a"}}]}],
                           "responses": [{"is": {"statusCode": 200}}]}, "two URL matchers")


class FixtureNotMutated(unittest.TestCase):
    """`bench_direct.py` is untouched by this change, and importing it must stay side-effect free —
    otherwise a wiremock run could perturb the rift/mb stability contract."""

    def test_translation_does_not_mutate_the_fixture(self):
        before = copy.deepcopy(bd.IMPOSTERS)
        scenarios_before = copy.deepcopy(bd.SCENARIOS)
        for _port, _name, stubs in bd.IMPOSTERS:
            bw.wiremock_mappings(stubs)
        self.assertEqual(bd.IMPOSTERS, before)
        self.assertEqual(bd.SCENARIOS, scenarios_before)

    def test_default_comparison_set_is_still_the_published_thirteen(self):
        self.assertEqual(len(bd.SCENARIOS), 13)


class JavaPreflight(unittest.TestCase):
    def test_parses_modern_and_legacy_version_strings(self):
        self.assertEqual(bw.parse_java_major('openjdk version "17.0.13" 2024-10-15'), 17)
        self.assertEqual(bw.parse_java_major('java version "21.0.4" 2024-07-16 LTS'), 21)
        self.assertEqual(bw.parse_java_major('java version "1.8.0_402"'), 8)

    def test_unparseable_is_none(self):
        self.assertIsNone(bw.parse_java_major("no version here"))

    def test_minimum_is_wiremock_3s_requirement(self):
        self.assertEqual(bw.MIN_JAVA_MAJOR, 11)


class Ports(unittest.TestCase):
    def test_offset_is_disjoint_from_rift_and_mb(self):
        self.assertEqual(bw.WIREMOCK_OFFSET, 200)
        wm = set(bw.instance_ports())
        rift = {p for p, _, _ in bd.IMPOSTERS}
        mb = {p + 100 for p, _, _ in bd.IMPOSTERS}
        self.assertFalse(wm & rift)
        self.assertFalse(wm & mb)

    def test_one_instance_per_imposter(self):
        self.assertEqual(len(bw.instance_ports()), len(bd.IMPOSTERS))

    def test_command_pins_the_port_and_disables_the_request_journal(self):
        # `--no-request-journal` is a fairness requirement: WireMock records every request by
        # default, unbounded, while rift and mb are both measured with recording off. Without it
        # the table compares WireMock-with-journaling against Rift-without.
        # No container_threads => stock defaults, which is exactly what the secondary series wants.
        self.assertEqual(bw.wiremock_cmd("/x/wm.jar", 4745),
                         ["java", "-jar", "/x/wm.jar", "--port", "4745", "--disable-banner",
                          "--no-request-journal"])

    def test_command_pins_the_container_pool_when_asked(self):
        self.assertEqual(bw.wiremock_cmd("/x/wm.jar", 4745, 64),
                         ["java", "-jar", "/x/wm.jar", "--port", "4745", "--disable-banner",
                          "--no-request-journal", "--container-threads", "64"])


class ContainerThreads(unittest.TestCase):
    """Issue #865. WireMock is thread-per-request, so its 10-thread default caps in-flight requests
    below the offered concurrency — benchmarking that measures the pool, not the engine."""

    def test_pins_above_the_offered_concurrency(self):
        # 200 connections must not be served by a cpu_count-sized pool.
        self.assertEqual(bw.container_thread_count([200]), max(os.cpu_count() or 1, 200))

    def test_never_drops_below_cpu_count(self):
        # At 1 connection the pool should still not be smaller than the machine.
        self.assertEqual(bw.container_thread_count([1]), max(os.cpu_count() or 1, 1))

    def test_sweep_covers_the_TOP_of_the_sweep(self):
        # The pin has to clear the largest point in the sweep, not the first or the last one
        # listed — otherwise the highest-concurrency point is the only one that gets throttled,
        # which is exactly where the ratio matters most.
        self.assertEqual(bw.container_thread_count([1, 200, 50]), max(os.cpu_count() or 1, 200))

    def test_override_wins(self):
        self.assertEqual(bw.container_thread_count([1000], override=12), 12)

    def test_override_must_be_positive(self):
        with self.assertRaises(SystemExit):
            bw.container_thread_count([50], override=0)

    def test_stock_default_constant_matches_wiremocks_documented_default(self):
        self.assertEqual(bw.STOCK_CONTAINER_THREADS, 10)

    def test_the_pin_is_recorded_at_run_time_and_read_back(self):
        # The documented flow is two commands (`--run-all`, then a separate `--report`), so the
        # report process cannot know what the run chose. Re-deriving it from the report
        # invocation's flags would state a number the run never used — e.g. run with
        # `--sweep-connections 50,200` (pin 200), report with `--connections 50` (would infer 50).
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self.assertIsNone(bw.recorded_container_threads(""))
                bw.record_container_threads("", 200)
                self.assertEqual(bw.recorded_container_threads(""), 200)
                bw.record_container_threads("_rep2", 64)
                self.assertEqual(bw.recorded_container_threads("_rep2"), 64)
                self.assertEqual(bw.recorded_container_threads(""), 200,
                                 "reps must not clobber each other's recorded pin")
            finally:
                bw.RESULTS_DIR = orig

    def test_unreadable_sidecar_reports_unknown_rather_than_guessing(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                with open(bw.threads_sidecar_path(""), "w") as f:
                    f.write("not a number")
                self.assertIsNone(bw.recorded_container_threads(""))
            finally:
                bw.RESULTS_DIR = orig

    def test_stock_series_has_its_own_engine_label(self):
        # Its own label keeps it in its own CSV and out of the headline ratio.
        self.assertEqual(bw.STOCK_ENGINE, "wiremock-stock")
        self.assertNotEqual(bw.STOCK_ENGINE, "wiremock")
        self.assertTrue(bw.engine_csv_path(bw.STOCK_ENGINE, "").endswith("direct_wiremock-stock.csv"))


class AggregateReps(unittest.TestCase):
    """Issue #866. A published number must not rest on one sample of each engine, and a median must
    not silently rest on fewer reps than it claims."""

    HEADER = bd.CSV_HEADER

    def _write_rep(self, tmp, engine, rep, rps, scenarios=None, conns=50):
        path = os.path.join(tmp, f"direct_{engine}_rep{rep}.csv")
        with open(path, "w") as f:
            f.write(self.HEADER + "\n")
            for s in (scenarios if scenarios is not None else [x[0] for x in bd.SCENARIOS]):
                f.write(f"{s},{conns},closed,{rps},1.0,2.0,3.0,4.0,1.5,,\n")
        return path

    def test_finds_rep_files_for_any_engine_not_just_rift(self):
        # bench_direct.find_rep_files hardcodes a `direct_rift` prefix, so it cannot see these.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write_rep(tmp, "wiremock", 1, 100.0)
                self._write_rep(tmp, "wiremock", 2, 200.0)
                self._write_rep(tmp, "wiremock-stock", 1, 50.0)
                self.assertEqual(len(bw.find_engine_rep_files("wiremock")), 2)
                self.assertEqual(len(bw.find_engine_rep_files("wiremock-stock")), 1)
                self.assertEqual(bw.find_engine_rep_files("mb"), [])
            finally:
                bw.RESULTS_DIR = orig

    def test_rep_files_are_ordered_numerically_not_lexically(self):
        # rep10 must sort after rep2. The median itself is order-independent, but the rep numbers
        # this ordering yields are what `propagate_run_settings` reads the per-rep sidecars by.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep in (1, 2, 10):
                    self._write_rep(tmp, "wiremock", rep, 100.0)
                got = [os.path.basename(p) for p in bw.find_engine_rep_files("wiremock")]
                self.assertEqual(got, ["direct_wiremock_rep1.csv", "direct_wiremock_rep2.csv",
                                       "direct_wiremock_rep10.csv"])
            finally:
                bw.RESULTS_DIR = orig

    def test_median_and_spread_are_written(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep, rps in ((1, 100.0), (2, 200.0), (3, 300.0)):
                    self._write_rep(tmp, "wiremock", rep, rps)
                path, n = bw.aggregate_engine("wiremock")
                self.assertEqual(n, 3)
                with open(path) as f:
                    rows = list(csv.DictReader(f))
            finally:
                bw.RESULTS_DIR = orig
        self.assertEqual(float(rows[0]["rps"]), 200.0, "median of 100/200/300")
        self.assertEqual(rows[0]["reps"], "3")
        # peak-to-peak (300-100) over mean (200) = 100%
        self.assertAlmostEqual(float(rows[0]["rps_spread_pct"]), 100.0, places=1)

    def test_a_point_missing_from_one_rep_is_a_hard_error(self):
        # The #773 rule: a complete-looking report whose cells rest on 2 of 3 reps, exit 0, nothing
        # said, is exactly the silence this must not reproduce.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write_rep(tmp, "wiremock", 1, 100.0)
                self._write_rep(tmp, "wiremock", 2, 200.0,
                                scenarios=[s[0] for s in bd.SCENARIOS][:-1])  # one point short
                with self.assertRaises(SystemExit) as ctx:
                    bw.aggregate_engine("wiremock")
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("incomplete repetitions", str(ctx.exception))

    def test_absent_engine_yields_none_not_an_error(self):
        # A run may legitimately not include Mountebank.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self.assertIsNone(bw.aggregate_engine("mb"))
            finally:
                bw.RESULTS_DIR = orig

    def test_aggregate_all_covers_both_wiremock_engines(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for eng in ("wiremock", "wiremock-stock", "rift"):
                    for rep in (1, 2):
                        self._write_rep(tmp, eng, rep, 100.0 * rep)
                done = bw.aggregate_all_reps()
                produced = sorted(os.path.basename(p) for p in
                                  glob.glob(os.path.join(tmp, "*_median.csv")))
            finally:
                bw.RESULTS_DIR = orig
        self.assertEqual(done, {"wiremock": 2, "wiremock-stock": 2, "rift": 2})
        self.assertEqual(produced, ["direct_rift_median.csv", "direct_wiremock-stock_median.csv",
                                    "direct_wiremock_median.csv"])

    def test_aggregate_all_with_no_wiremock_reps_is_an_error(self):
        # Producing nothing and exiting 0 would read as success.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write_rep(tmp, "rift", 1, 100.0)
                with self.assertRaises(SystemExit) as ctx:
                    bw.aggregate_all_reps()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("no tuned WireMock rep files", str(ctx.exception))

    def test_median_csv_is_readable_by_the_report_loader(self):
        # The whole point: --aggregate-reps then --report --csv-suffix _median.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep, rps in ((1, 100.0), (2, 300.0)):
                    self._write_rep(tmp, "wiremock", rep, rps)
                bw.aggregate_engine("wiremock")
                rows = bw.load_engine_csv("wiremock", "_median", 50)
            finally:
                bw.RESULTS_DIR = orig
        self.assertIsNotNone(rows)
        self.assertEqual(rows["simple_health"]["rps"], 200.0)
        self.assertEqual(rows["simple_health"]["reps"], "2")

    def test_the_reps_thread_pin_reaches_the_median_suffix(self):
        # Without this the median report says "unrecorded" for a pin every rep actually ran with,
        # and #865's requirement (state the value used) is lost the moment reps are aggregated.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep in (1, 2):
                    self._write_rep(tmp, "wiremock", rep, 100.0 * rep)
                    bw.record_container_threads(f"_rep{rep}", 16)
                bw.aggregate_all_reps()
                got = bw.recorded_container_threads("_median")
            finally:
                bw.RESULTS_DIR = orig
        self.assertEqual(got, 16)

    def test_reps_that_disagree_on_the_pin_are_a_hard_error(self):
        # Different Jetty pool sizes are different configurations; their median describes none.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep, threads in ((1, 16), (2, 10)):
                    self._write_rep(tmp, "wiremock", rep, 100.0)
                    bw.record_container_threads(f"_rep{rep}", threads)
                with self.assertRaises(SystemExit) as ctx:
                    bw.aggregate_all_reps()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("disagree on --container-threads", str(ctx.exception))

    def test_engines_with_unequal_rep_counts_are_a_hard_error(self):
        # bench_direct refuses this for rift-vs-mb; the 3-way table is the more-quoted artefact and
        # had no equivalent guard. Worse than unfair weighting: the one-rep column reports 0.0%
        # spread, so the thinnest data renders as the steadiest engine.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write_rep(tmp, "rift", 1, 9000.0)
                for rep in (1, 2, 3):
                    self._write_rep(tmp, "wiremock", rep, 3000.0)
                with self.assertRaises(SystemExit) as ctx:
                    bw.aggregate_all_reps()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("rep-count mismatch", str(ctx.exception))
        self.assertIn("rift=1", str(ctx.exception))

    def test_stock_reps_alone_cannot_stand_in_for_the_tuned_series(self):
        # The headline ratio comes from the tuned series; aggregating only the stock one and
        # printing a success line would misrepresent what the run can publish.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep in (1, 2):
                    self._write_rep(tmp, "wiremock-stock", rep, 900.0)
                with self.assertRaises(SystemExit) as ctx:
                    bw.aggregate_all_reps()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("no tuned WireMock rep files", str(ctx.exception))

    def test_the_reps_warmup_reaches_the_median_suffix(self):
        # The warmup is the setting the 3-way table claims to hold equal, so losing it at
        # aggregation is what makes the published claim uncheckable.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep in (1, 2):
                    self._write_rep(tmp, "wiremock", rep, 100.0 * rep)
                    bw.record_warmup(f"_rep{rep}", "10s")
                bw.aggregate_all_reps()
                got = bw.recorded_warmup("_median")
            finally:
                bw.RESULTS_DIR = orig
        self.assertEqual(got, "10s")

    def test_reps_that_disagree_on_the_warmup_are_a_hard_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep, warm in ((1, "10s"), (2, "3s")):
                    self._write_rep(tmp, "wiremock", rep, 100.0)
                    bw.record_warmup(f"_rep{rep}", warm)
                with self.assertRaises(SystemExit) as ctx:
                    bw.aggregate_all_reps()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("disagree on --warmup", str(ctx.exception))

    def test_the_pin_is_recovered_when_only_the_stock_series_survived(self):
        # Both series share one sidecar per rep. Enumerating rep numbers from the tuned series
        # alone loses the pin whenever a partial re-run left only the stock CSVs behind.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep in (1, 2):
                    self._write_rep(tmp, "wiremock", rep, 100.0)
                    self._write_rep(tmp, "wiremock-stock", rep, 50.0)
                    bw.record_container_threads(f"_rep{rep}", 16)
                os.remove(os.path.join(tmp, "direct_wiremock_rep1.csv"))
                os.remove(os.path.join(tmp, "direct_wiremock_rep2.csv"))
                self.assertEqual(bw.propagate_run_settings(""), {"threads": "16"})
                got = bw.recorded_container_threads("_median")
            finally:
                bw.RESULTS_DIR = orig
        self.assertEqual(got, 16)

    def test_unrecorded_pin_stays_unrecorded_rather_than_invented(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep in (1, 2):
                    self._write_rep(tmp, "wiremock", rep, 100.0)
                bw.aggregate_all_reps()
                got = bw.recorded_container_threads("_median")
            finally:
                bw.RESULTS_DIR = orig
        self.assertIsNone(got)


class ReportSpread(unittest.TestCase):
    """AC3. The spread table is the part of #866 a reader actually sees: it is what stops a median
    over a degraded rep from reading like a clean number."""

    def _median_csv(self, tmp, engine, rps, reps, spread):
        path = os.path.join(tmp, f"direct_{engine}_median.csv")
        with open(path, "w") as f:
            f.write(bd.CSV_HEADER + ",reps,rps_spread_pct\n")
            for s, *_ in bd.SCENARIOS:
                f.write(f"{s},50,closed,{rps},1.0,2.0,3.0,4.0,1.5,,,{reps},{spread}\n")

    def _render(self, tmp, series, **kw):
        for engine, rps, reps, spread in series:
            self._median_csv(tmp, engine, rps, reps, spread)
        out = bw.report("local", "2.9.1", "3.9.1", "openjdk 21", "20s", 50,
                        csv_suffix="_median", **kw)
        return open(out).read()

    def _in_tmp(self, series, **kw):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                return self._render(tmp, series, **kw)
            finally:
                bw.RESULTS_DIR = orig

    def test_the_spread_table_is_rendered_for_medians(self):
        text = self._in_tmp([("rift", 60000.0, 3, "4.2"), ("wiremock", 20000.0, 3, "11.5")])
        self.assertIn("## Repetition spread", text)
        self.assertIn("4.2%", text)
        self.assertIn("11.5%", text)

    def test_a_single_rep_reports_n_a_not_zero_percent(self):
        # Peak-to-peak over one sample is 0.0%, so a one-rep column would render as the STEADIEST
        # engine in a table whose entire purpose is to expose thin replication.
        text = self._in_tmp([("rift", 60000.0, 1, "0.0"), ("wiremock", 20000.0, 3, "11.5")])
        spread = text.split("## Repetition spread")[1]
        row = [l for l in spread.splitlines() if l.startswith("| simple_health |")][0]
        self.assertIn("n/a", row)
        self.assertNotIn("0.0%", row)

    def test_each_column_states_its_own_rep_count(self):
        # "a median of 1, 3 repetitions" does not say WHICH engine got one.
        text = self._in_tmp([("rift", 60000.0, 1, "0.0"), ("wiremock", 20000.0, 3, "11.5")])
        header = [l for l in text.split("## Repetition spread")[1].splitlines()
                  if l.startswith("| Scenario |")][0]
        self.assertIn("Rift (n=1)", header)
        self.assertIn("WireMock (n=3)", header)

    def test_a_single_rep_report_has_no_spread_table_at_all(self):
        # A plain (non-aggregated) run must render exactly as it did before #866.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for engine, rps in (("rift", 60000.0), ("wiremock", 20000.0)):
                    path = os.path.join(tmp, f"direct_{engine}.csv")
                    with open(path, "w") as f:
                        f.write(bd.CSV_HEADER + "\n")
                        for s, *_ in bd.SCENARIOS:
                            f.write(f"{s},50,closed,{rps},1.0,2.0,3.0,4.0,1.5,,\n")
                out = bw.report("local", "2.9.1", "3.9.1", "openjdk 21", "20s", 50)
                text = open(out).read()
            finally:
                bw.RESULTS_DIR = orig
        self.assertNotIn("## Repetition spread", text)

    def test_the_warmup_reaches_the_method_section(self):
        # #866's premise: a 3s-warmed Rift against a 10s-warmed JVM is not a comparison. A reader
        # who cannot see the warmup cannot check that claim.
        text = self._in_tmp([("rift", 60000.0, 3, "4.2"), ("wiremock", 20000.0, 3, "11.5")],
                            warmup="10s")
        self.assertIn("10s warmup", text)

    def test_an_unrecorded_warmup_is_admitted_rather_than_defaulted(self):
        text = self._in_tmp([("rift", 60000.0, 3, "4.2"), ("wiremock", 20000.0, 3, "11.5")])
        self.assertIn("**unrecorded** warmup", text)
        self.assertNotIn("10s warmup", text)


class ReportSuffixChaining(unittest.TestCase):
    """`--aggregate-reps --report` in one invocation is exactly what benchmark-publish.yml runs.
    The suffix hand-off between the two lives in `__main__`, so it is extracted to be testable —
    without this the report would silently render the un-aggregated single-rep CSVs."""

    def test_rep_tag_is_the_default_suffix(self):
        self.assertEqual(bw.resolve_suffix(None, 3), "_rep3")

    def test_explicit_suffix_wins_over_no_rep(self):
        self.assertEqual(bw.resolve_suffix("_variant", None), "_variant")

    def test_no_rep_and_no_suffix_is_unsuffixed(self):
        self.assertEqual(bw.resolve_suffix(None, None), "")

    def test_aggregation_writes_where_the_report_then_reads(self):
        # The contract that makes `--aggregate-reps --report` work as one command.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                for rep in (1, 2):
                    for engine in ("rift", "wiremock"):
                        path = os.path.join(tmp, f"direct_{engine}_rep{rep}.csv")
                        with open(path, "w") as f:
                            f.write(bd.CSV_HEADER + "\n")
                            for s, *_ in bd.SCENARIOS:
                                f.write(f"{s},50,closed,{100.0 * rep},1.0,2.0,3.0,4.0,1.5,,\n")
                bw.aggregate_all_reps("")
                suffix = bw.median_suffix("")
                self.assertIsNotNone(bw.load_engine_csv("wiremock", suffix, 50),
                                     "the report must find what the aggregation just wrote")
            finally:
                bw.RESULTS_DIR = orig
        self.assertEqual(suffix, "_median")


class PublishWorkflow(unittest.TestCase):
    """Issue #866. The publication workflow is where a like-for-like comparison is won or lost, and
    the failure mode is silent: a table whose engines were warmed differently still renders."""

    WORKFLOW = os.path.join(os.path.dirname(__file__), "..", "..", "..",
                            ".github", "workflows", "benchmark-publish.yml")

    @classmethod
    def setUpClass(cls):
        with open(cls.WORKFLOW) as f:
            cls.text = f.read()

    def test_all_three_engines_are_warmed_by_one_and_the_same_expression(self):
        # The whole point of issue #866. Capturing to end-of-line, not to the first space: a regex
        # that stops at whitespace matches the shared `"${{` prefix of ANY expression, so one leg
        # silently warmed from a different variable would still look identical.
        runs = []
        for line in self.text.splitlines():
            if line.lstrip().startswith("#"):
                continue
            m = re.search(r"--warmup\s+(.+)$", line)
            if m:
                runs.append(m.group(1).strip().rstrip("\\").strip())
        self.assertEqual(len(runs), 3, f"expected 3 benched legs, saw {runs}")
        self.assertEqual(len(set(runs)), 1,
                         f"the legs are warmed differently, so the ratio is not like-for-like: {runs}")

    def test_no_leg_hardcodes_a_warmup_duration(self):
        self.assertNotRegex(self.text, r"--warmup\s+\d+[smh]\b")

    def test_the_warmup_the_legs_use_is_bound_to_the_dispatch_input(self):
        # Guards the other half: three legs could agree on a variable that is wired to `duration`,
        # or to nothing at all (unset + `set -u` would at least fail loudly, but a typo'd binding
        # to another input would not).
        warmup_var = re.search(r"--warmup\s+\"\$(\w+)\"", self.text)
        self.assertIsNotNone(warmup_var, "the legs should take the warmup from an env var")
        self.assertRegex(
            self.text,
            rf"{warmup_var.group(1)}:\s*\$\{{\{{\s*inputs\.warmup\s*\}}\}}",
            "the legs' warmup variable must be bound to the `warmup` dispatch input")

    def test_dispatch_inputs_are_not_interpolated_into_shell_bodies(self):
        # `${{ }}` is substituted before bash parses the line, so an input reaching a run: body is
        # script injection. Step `name:` fields are not shell and are excluded.
        offenders = [l.strip() for l in self.text.splitlines()
                     if "${{ inputs." in l and not l.lstrip().startswith(("- name:", "#"))
                     and ":" not in l.split("${{")[0].rstrip()[-1:]]
        self.assertEqual(offenders, [], f"inputs reach the shell directly: {offenders}")

    def test_wiremock_version_is_pinned_by_input_not_by_the_jar_that_happened_to_be_there(self):
        self.assertIn("wiremock_version:", self.text)
        self.assertIn("--wiremock-version", self.text)
        self.assertNotIn("/wiremock-standalone/3.9.1/", self.text,
                         "the download URL must interpolate the input, not a frozen version")

    def test_wiremock_jvm_logs_are_collected(self):
        # 14 JVMs write to results/logs/; the flat results/*.log globs do not reach them, and this
        # is the leg most likely to die on a readiness probe or an OOM.
        self.assertIn("tests/benchmark/results/logs/*.log", self.text)
        self.assertIn("results/logs/*.log", self.text.split("Diagnostics on failure")[1])

    def test_the_sweep_can_be_skipped_without_touching_the_comparison_legs(self):
        # The sweep is ~75% of the wall clock and contributes nothing to the 3-way table, so a
        # WireMock-only dispatch must be able to turn it off — and ONLY it. Gating a comparison
        # leg by mistake would publish a table missing an engine.
        self.assertIn("run_sweep:", self.text)
        gated = [l for l in self.text.splitlines() if "if: inputs.run_sweep" in l]
        self.assertEqual(len(gated), 1, "exactly one leg may be gated by run_sweep")
        sweep_at = self.text.index("Rift concurrency sweep")
        gate_at = self.text.index("if: inputs.run_sweep")
        self.assertLess(
            abs(self.text[:gate_at].count("\n") - self.text[:sweep_at].count("\n")), 3,
            "the run_sweep gate must sit on the sweep step, not on a comparison leg")

    def test_the_parked_comparison_medians_do_not_collide_with_the_sweeps(self):
        # results/direct_rift_median.csv means two different things (c=50 comparison vs the sweep's
        # 1/50/256/512) and only one is the basis of the published ratio.
        self.assertIn("direct_rift_comparison_median.csv", self.text)
        self.assertIn("direct_mb_comparison_median.csv", self.text)

    def test_the_wiremock_leg_parks_its_medians_before_the_sweep_overwrites_them(self):
        # The sweep re-runs bench_direct --aggregate-reps, which writes direct_rift_median.csv.
        # Without the park, the published 3-way table is silently rebuilt from sweep data.
        wm = self.text.index("bench_wiremock.py --run-all")
        sweep = self.text.index("--sweep-connections")
        self.assertLess(wm, sweep, "the WireMock leg must run before the Rift-only sweep")
        park = self.text.index("results/wiremock/")
        self.assertLess(park, sweep, "medians must be parked before the sweep runs")


class ReportCombiner(unittest.TestCase):
    HEADER = bd.CSV_HEADER

    def _write(self, tmp, engine, rows, suffix=""):
        path = os.path.join(tmp, f"direct_{engine}{suffix}.csv")
        with open(path, "w") as f:
            f.write(self.HEADER + "\n")
            for r in rows:
                f.write(r + "\n")
        return path

    def _row(self, scenario, conns=50, mode="closed", rps=1000.0):
        return f"{scenario},{conns},{mode},{rps},1.0,2.0,3.0,4.0,1.5,,"

    def _all_scenarios(self, conns=50, mode="closed", rps=1000.0):
        return [self._row(s[0], conns, mode, rps) for s in bd.SCENARIOS]

    def test_renders_na_when_mountebank_csv_is_absent(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios(rps=50000.0))
                self._write(tmp, "wiremock", self._all_scenarios(rps=10000.0))
                out = bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50)
                text = open(out).read()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("n/a", text)
        self.assertIn("not measured on this box", text)
        self.assertIn("5.0x", text)  # Rift/WM speedup still rendered

    def test_three_way_table_when_all_three_present(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios(rps=60000.0))
                self._write(tmp, "wiremock", self._all_scenarios(rps=20000.0))
                self._write(tmp, "mb", self._all_scenarios(rps=6000.0))
                out = bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50)
                text = open(out).read()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("| Scenario | Mountebank | WireMock | Rift | Rift/MB | Rift/WM |", text)
        self.assertIn("10.0x", text)  # 60000/6000
        self.assertIn("3.0x", text)   # 60000/20000

    def test_refuses_a_csv_whose_connections_do_not_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios(conns=50))
                self._write(tmp, "wiremock", self._all_scenarios(conns=200))
                with self.assertRaises(SystemExit) as ctx:
                    bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50)
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("wiremock", str(ctx.exception))

    def test_stock_column_appears_but_never_drives_the_speedup(self):
        # Issue #865's core reporting contract: the stock series is visible as its own labelled
        # column, and the Rift/WM ratio comes from the TUNED series. A speedup computed against a
        # 10-thread-throttled WireMock would measure the pool, and is the first thing a WireMock
        # user would rightly reject.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios(rps=60000.0))
                self._write(tmp, "wiremock", self._all_scenarios(rps=20000.0))       # tuned
                self._write(tmp, "wiremock-stock", self._all_scenarios(rps=5000.0))  # stock
                out = bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50,
                                container_threads=64)
                text = open(out).read()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("WireMock (stock, 10t)", text)
        self.assertIn("5,000", text)
        self.assertIn("3.0x", text)   # 60000/20000 — the tuned ratio
        self.assertNotIn("12.0x", text, "the ratio must NOT be computed against the stock series")
        # Assert the pin reached the Method section without pinning the exact prose around it.
        self.assertRegex(text, r"\*\*64\*\* Jetty container threads")

    def test_both_wiremock_columns_are_labelled_with_their_thread_count(self):
        # Two adjacent WireMock columns where only one carries its thread count would read as
        # "stock vs default" — the exact confusion #865 exists to remove.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios(rps=60000.0))
                self._write(tmp, "wiremock", self._all_scenarios(rps=20000.0))
                self._write(tmp, "wiremock-stock", self._all_scenarios(rps=5000.0))
                out = bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50,
                                container_threads=64)
                header = [l for l in open(out) if l.startswith("| Scenario |")][0]
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("WireMock (stock, 10t)", header)
        self.assertIn("WireMock (64t)", header)

    def test_report_says_unrecorded_rather_than_inventing_a_thread_count(self):
        # If neither the run nor the caller recorded a pin, the Method section must say so — a
        # plausible-looking wrong number is worse than an admitted gap.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios(rps=60000.0))
                self._write(tmp, "wiremock", self._all_scenarios(rps=20000.0))
                out = bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50,
                                container_threads=None)
                text = open(out).read()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("unrecorded", text)
        self.assertIn("--container-threads", text)

    def test_report_discloses_that_the_pin_may_be_a_no_op(self):
        # Measured on a 10-core box, pinning to 50 made no difference against the 10-thread
        # default: the CPU saturates first. A report that asserted "measuring stock measures the
        # pool, not the engine" while the stock column matched the headline would contradict
        # itself, so the wording has to admit the pin may not bind.
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios(rps=60000.0))
                self._write(tmp, "wiremock", self._all_scenarios(rps=20000.0))
                out = bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50,
                                container_threads=64)
                text = open(out).read()
            finally:
                bw.RESULTS_DIR = orig
        self.assertIn("fairness guarantee, not a speedup", text)
        self.assertIn("no-op", text)

    def test_report_without_a_stock_csv_omits_the_column(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios(rps=60000.0))
                self._write(tmp, "wiremock", self._all_scenarios(rps=20000.0))
                out = bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50,
                                container_threads=64)
                text = open(out).read()
            finally:
                bw.RESULTS_DIR = orig
        self.assertNotIn("stock", text.split("## Throughput")[1])
        self.assertIn("3.0x", text)

    def test_refuses_open_loop_rows_for_the_closed_loop_table(self):
        with tempfile.TemporaryDirectory() as tmp:
            orig = bw.RESULTS_DIR
            bw.RESULTS_DIR = tmp
            try:
                self._write(tmp, "rift", self._all_scenarios())
                self._write(tmp, "wiremock", self._all_scenarios(mode="open"))
                with self.assertRaises(SystemExit):
                    bw.report("local", "2.9.1", "3.9.1", "openjdk 17", "20s", 50)
            finally:
                bw.RESULTS_DIR = orig


if __name__ == "__main__":
    unittest.main()
