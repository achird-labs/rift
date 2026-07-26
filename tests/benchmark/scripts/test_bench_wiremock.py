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
        self.assertEqual(bw.wiremock_cmd("/x/wm.jar", 4745),
                         ["java", "-jar", "/x/wm.jar", "--port", "4745", "--disable-banner",
                          "--no-request-journal"])


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
