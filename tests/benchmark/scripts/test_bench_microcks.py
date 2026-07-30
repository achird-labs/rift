#!/usr/bin/env python3
"""Tests for the Microcks suite (issue #900).

The thing under test is the **translator**, because it is the only part of this suite that can be
wrong in a way that still produces a plausible published number. A launch bug fails loudly; a
translation bug produces a Microcks that serves the right body for the six measured scenarios while
holding a thinner or differently-shaped corpus than Rift did, and the run reports it as fast.

So the properties pinned here are the ones the report's fairness claims rest on:

  * bodies come out byte-identical to Rift's, so `EXPECT_BODY` stays a strong assertion;
  * the operation count matches the stub count for the path/verb family;
  * query-discriminated stubs collapse to ONE operation with N named examples, which is what makes
    Microcks infer `URI_PARAMS` — get this wrong and the 100-way dispatch silently becomes 1-way;
  * every fixture scenario is either measured or explicitly refused with a reason;
  * an untranslatable predicate is fatal, never skipped.

Run:
    python3 -m unittest test_bench_microcks -v
"""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import bench_direct  # noqa: E402
import bench_microcks as bm  # noqa: E402


class TranslatesPathVerbFamily(unittest.TestCase):
    """The shape the stub-growth claim is measured on: distinct literal paths, one example each."""

    def test_simple_imposter_becomes_one_operation_per_stub(self):
        stubs = bench_direct.simple_stubs()
        spec = bm.openapi_spec("Simple", stubs)
        ops = [(p, verb) for p, item in spec["paths"].items() for verb in item]
        self.assertEqual(len(ops), len(stubs))
        self.assertEqual(sorted(p for p, _ in ops), ["/health", "/ping"])

    def test_api_imposter_operation_count_matches_stub_count(self):
        stubs = bench_direct.api_stubs()
        spec = bm.openapi_spec("API", stubs)
        ops = sum(len(item) for item in spec["paths"].values())
        self.assertEqual(ops, len(stubs))
        self.assertEqual(ops, bm.expected_operations(stubs))

    def test_methods_on_a_shared_path_stay_separate_operations(self):
        """`/api/v1/resource1/1` carries GET, PUT and DELETE. Collapsing them would drop two thirds
        of the corpus while every measured scenario still passed."""
        spec = bm.openapi_spec("API", bench_direct.api_stubs(resources=1, per=1))
        self.assertEqual(sorted(spec["paths"]["/api/v1/resource1/1"]),
                         ["delete", "get", "put"])

    def test_status_code_is_carried_through(self):
        spec = bm.openapi_spec("API", bench_direct.api_stubs(resources=1, per=1))
        self.assertIn("204", spec["paths"]["/api/v1/resource1/1"]["delete"]["responses"])

    def test_204_carries_no_content_block(self):
        """A 204 with an empty example makes Microcks emit a Content-Type on a bodiless response,
        which Rift does not do."""
        op = bm.openapi_spec("API", bench_direct.api_stubs(resources=1, per=1))[
            "paths"]["/api/v1/resource1/1"]["delete"]
        self.assertNotIn("content", op["responses"]["204"])

    def test_operation_ids_are_unique(self):
        spec = bm.openapi_spec("API", bench_direct.api_stubs())
        ids = [op["operationId"] for item in spec["paths"].values() for op in item.values()]
        self.assertEqual(len(ids), len(set(ids)))


class BodiesAreByteIdenticalToRift(unittest.TestCase):
    """The property that lets this suite reuse `EXPECT_BODY` unchanged.

    Microcks serves a *string* example verbatim but re-serializes a parsed object, and the fixture
    builds bodies with `json.dumps` (`{"id": 1}`, with spaces) while a JSON serializer would emit
    `{"id":1}`. If these tests ever fail, the fix is NOT to weaken `EXPECT_BODY` to a
    whitespace-insensitive match — it is to restore the raw-string example."""

    def test_json_example_value_is_the_raw_fixture_string(self):
        stubs = bench_direct.api_stubs(resources=1, per=1)
        spec = bm.openapi_spec("API", stubs)
        example = (spec["paths"]["/api/v1/resource1/1"]["get"]["responses"]["200"]
                   ["content"]["application/json"]["examples"])
        value = next(iter(example.values()))["value"]
        self.assertIsInstance(value, str)
        self.assertEqual(value, json.dumps({"id": 1, "name": "resource1_1"}))
        self.assertIn('"name": "resource1_1"', value)

    def test_marker_from_expect_body_is_present_in_the_generated_example(self):
        """The end-to-end version of the property: the exact substring `verify_body` will look for
        must appear in the bytes Microcks is being told to serve."""
        spec = bm.openapi_spec("API", bench_direct.api_stubs())
        example = (spec["paths"]["/api/v1/resource5/5"]["get"]["responses"]["200"]
                   ["content"]["application/json"]["examples"])
        value = next(iter(example.values()))["value"]
        self.assertIn(bench_direct.EXPECT_BODY["api_middle"], value)

    def test_plain_text_body_is_filed_under_text_plain(self):
        """`/health` -> `OK` sets no Content-Type. Filing it as JSON makes Microcks reject the
        artifact, which would look like a launch failure rather than a translation one."""
        spec = bm.openapi_spec("Simple", bench_direct.simple_stubs())
        content = spec["paths"]["/health"]["get"]["responses"]["200"]["content"]
        self.assertEqual(list(content), ["text/plain"])
        self.assertEqual(next(iter(content["text/plain"]["examples"].values()))["value"], "OK")

    def test_declared_content_type_wins_over_inference(self):
        stub = {"predicates": [{"equals": {"method": "GET", "path": "/x"}}],
                "responses": [{"is": {"statusCode": 200,
                                      "headers": {"Content-Type": "application/xml; charset=utf-8"},
                                      "body": "<a>1</a>"}}]}
        content = bm.openapi_spec("S", [stub])["paths"]["/x"]["get"]["responses"]["200"]["content"]
        self.assertEqual(list(content), ["application/xml"])

    def test_parsed_body_is_refused_rather_than_reserialized(self):
        """Guessing a separator here is exactly how the bodies would drift out of sync."""
        stub = {"predicates": [{"equals": {"method": "GET", "path": "/x"}}],
                "responses": [{"is": {"statusCode": 200, "body": {"id": 1}}}]}
        with self.assertRaises(SystemExit) as ctx:
            bm.openapi_spec("S", [stub])
        self.assertIn("pre-serialized", str(ctx.exception))


class QueryDispatchTranslation(unittest.TestCase):
    """100 query-`equals` stubs must become ONE operation with 100 named examples.

    This is the translation most likely to be wrong in a way that reads as a Microcks win: if the
    request examples are dropped, Microcks has one response and answers every `page` with it —
    a 1-way dispatch measured under a 100-way label."""

    def setUp(self):
        self.stubs = bench_direct.query_stubs()
        self.spec = bm.openapi_spec("Query", self.stubs)
        self.op = self.spec["paths"]["/query/search"]["get"]

    def test_collapses_to_a_single_operation(self):
        self.assertEqual(list(self.spec["paths"]), ["/query/search"])
        self.assertEqual(list(self.spec["paths"]["/query/search"]), ["get"])

    def test_one_response_example_per_stub(self):
        examples = self.op["responses"]["200"]["content"]["application/json"]["examples"]
        self.assertEqual(len(examples), len(self.stubs))

    def test_query_parameters_are_required_so_microcks_derives_dispatch_rules(self):
        params = {p["name"]: p for p in self.op["parameters"]}
        self.assertEqual(sorted(params), ["page", "size"])
        for p in params.values():
            self.assertTrue(p["required"], "an optional parameter is left out of URI_PARAMS rules")
            self.assertEqual(p["in"], "query")

    def test_request_and_response_examples_share_names(self):
        """Microcks pairs a request example with a response example by NAME. Divergent names mean no
        pairing, and the dispatcher silently falls back to a single response."""
        resp_names = set(self.op["responses"]["200"]["content"]["application/json"]["examples"])
        for param in self.op["parameters"]:
            self.assertEqual(set(param["examples"]), resp_names)

    def test_example_values_are_the_fixture_values(self):
        params = {p["name"]: p for p in self.op["parameters"]}
        last = bm._example_name(len(self.stubs))
        self.assertEqual(params["page"]["examples"][last]["value"], str(len(self.stubs)))
        self.assertEqual(params["size"]["examples"][last]["value"], "10")
        resp = self.op["responses"]["200"]["content"]["application/json"]["examples"][last]["value"]
        self.assertIn(bench_direct.EXPECT_BODY["query_last"], resp)

    def test_path_only_operations_carry_no_parameters_key(self):
        spec = bm.openapi_spec("Simple", bench_direct.simple_stubs())
        self.assertNotIn("parameters", spec["paths"]["/health"]["get"])


class UntranslatablePredicatesAreFatal(unittest.TestCase):
    """`_fail` exists so a stub nobody understood cannot be quietly dropped: the six measured
    scenarios would still pass while the surrounding corpus went missing."""

    def _reject(self, stub):
        with self.assertRaises(SystemExit):
            bm.openapi_spec("S", [stub])

    def test_regex_path_is_refused(self):
        self._reject(bench_direct.regex_stubs(1)[0])

    def test_header_predicate_is_refused(self):
        self._reject(bench_direct.header_stubs(1)[0])

    def test_body_predicate_is_refused(self):
        self._reject(bench_direct.json_body_stubs(1)[0])

    def test_and_or_predicate_is_refused(self):
        self._reject(bench_direct.complex_stubs(1)[0])

    def test_multi_predicate_stub_is_refused(self):
        self._reject(bench_direct.jsonpath_stubs(1)[0])

    def test_stub_without_a_path_is_refused(self):
        self._reject({"predicates": [{"equals": {"method": "GET"}}]},)

    def test_multiple_responses_are_refused(self):
        self._reject({"predicates": [{"equals": {"path": "/x"}}],
                      "responses": [{"is": {"statusCode": 200}}, {"is": {"statusCode": 201}}]})


class ScenarioClassification(unittest.TestCase):
    """Every fixture scenario is measured or refused — never silently absent from a report that
    claims to cover the suite."""

    def test_every_scenario_is_classified(self):
        bm.check_scenario_coverage()   # raises if not

    def test_comparable_and_untranslatable_are_disjoint(self):
        self.assertEqual(set(bm.COMPARABLE) & set(bm.UNTRANSLATABLE), set())

    def test_classification_covers_exactly_the_fixture(self):
        self.assertEqual(set(bm.COMPARABLE) | set(bm.UNTRANSLATABLE),
                         {s[0] for s in bench_direct.SCENARIOS})

    def test_every_exclusion_states_a_reason(self):
        for name, reason in bm.UNTRANSLATABLE.items():
            self.assertGreater(len(reason), 60, f"{name}'s exclusion reason is too thin to publish")

    def test_comparable_scenarios_are_drawn_from_the_fixture(self):
        rows = bm.comparable_scenarios()
        self.assertEqual([r[0] for r in rows],
                         [s[0] for s in bench_direct.SCENARIOS if s[0] in bm.COMPARABLE])

    def test_every_comparable_scenario_has_an_expected_body_marker(self):
        for name in bm.COMPARABLE:
            self.assertIn(name, bench_direct.EXPECT_BODY)

    def test_scenario_groups_share_a_launch_per_imposter(self):
        """The four API scenarios must land in one group, or the run pays a JVM start per scenario."""
        groups = dict(bm.scenario_groups())
        api = [r[0] for r in groups[4545]]
        self.assertEqual(api, ["api_first", "api_middle", "api_last", "no_match"])

    def test_every_group_maps_to_a_known_imposter(self):
        by_port = bm.imposter_by_port()
        for base_port, _rows in bm.scenario_groups():
            self.assertIn(base_port, by_port)


class NoMatchIsMeasuredAsA404(unittest.TestCase):
    """Microcks has no catch-all, so `no_match` is a 404. The gate must expect that specific status
    rather than being relaxed to "any status", which would stop catching a mis-served stub."""

    def test_no_match_expects_a_4xx(self):
        self.assertEqual(bm.EXPECT_STATUS_PREFIX["no_match"], "4")
        self.assertTrue(bm._status_ok("no_match", {"404": 100}))
        self.assertFalse(bm._status_ok("no_match", {"200": 100}))

    def test_other_scenarios_still_require_2xx(self):
        self.assertTrue(bm._status_ok("api_last", {"200": 10}))
        self.assertFalse(bm._status_ok("api_last", {"404": 10}))
        self.assertFalse(bm._status_ok("api_last", {"500": 10}))

    def test_empty_distribution_is_not_ok(self):
        self.assertFalse(bm._status_ok("api_last", {}))

    def test_no_match_body_marker_is_still_the_empty_default(self):
        """The 404 body is empty, so `EXPECT_BODY[no_match] is None` remains a real assertion that
        nothing matched — the status difference does not weaken the body gate."""
        self.assertIsNone(bench_direct.EXPECT_BODY["no_match"])


class FairnessKnobs(unittest.TestCase):
    def test_tomcat_threads_never_below_offered_concurrency(self):
        """Tomcat's default is 200 while the published table drives 256 — the exact trap the
        WireMock leg's container-thread pin exists for."""
        self.assertGreaterEqual(bm.tomcat_thread_count([256]), 256)
        self.assertGreaterEqual(bm.tomcat_thread_count([50, 256]), 256)

    def test_tomcat_threads_at_least_core_count(self):
        self.assertGreaterEqual(bm.tomcat_thread_count([1]), os.cpu_count() or 1)

    def test_tomcat_threads_override_is_honoured(self):
        self.assertEqual(bm.tomcat_thread_count([50], override=999), 999)

    def test_tomcat_threads_override_rejects_nonsense(self):
        with self.assertRaises(SystemExit):
            bm.tomcat_thread_count([50], override=0)

    def test_command_pins_heap_on_both_bounds(self):
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 256, heap="4g")
        self.assertIn("-Xms4g", cmd)
        self.assertIn("-Xmx4g", cmd)

    def test_command_selects_the_standalone_profile(self):
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 256)
        self.assertIn("-Dspring.profiles.active=uber", cmd)

    def test_tuned_series_disables_invocation_stats(self):
        """The whole reason this class matters. `mocks.enable-invocation-stats` defaults to ON and
        persists a record per mock call — the analogue of WireMock's request journal, which is
        disabled in its leg. Leaving it on would compare Microcks-with-recording against
        Rift-without, and unlike every other deviation here that error flatters RIFT."""
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 256)
        self.assertIn("--mocks.enable-invocation-stats=false", cmd)

    def test_tuned_series_disables_the_cors_policy(self):
        """Four Access-Control-* headers on every response that neither Rift nor WireMock emits."""
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 256)
        self.assertIn("--mocks.rest.enable-cors-policy=false", cmd)

    def test_stock_series_restores_every_default_it_claims_to(self):
        """A "stock defaults" column that quietly keeps a tune is worse than no column."""
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 256, stock=True)
        for flag in bm.FAIRNESS_FLAGS:
            self.assertNotIn(flag, cmd)
        self.assertFalse([c for c in cmd if c.startswith("--server.tomcat.threads.max")],
                         "the stock series must not pin the pool")

    def test_stock_series_still_pins_heap_and_logging(self):
        """Determinism knobs, not fairness ones: floating them would make the stock column
        non-comparable between hosts, and INFO logging would measure the logging pipeline (#718)."""
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 256, heap="4g", stock=True)
        self.assertIn("-Xmx4g", cmd)
        self.assertIn("--logging.level.root=WARN", cmd)

    def test_stock_engine_label_is_distinct(self):
        """It must not enter the headline Rift/Microcks ratio."""
        self.assertEqual(bm.STOCK_ENGINE, "microcks-stock")
        self.assertNotEqual(bm.STOCK_ENGINE, "microcks")

    def test_command_disables_asyncapi_and_lowers_logging(self):
        """Per #718: a per-request log site turns a throughput benchmark into a measurement of the
        logging pipeline. The same trap applies to every engine, not just Rift."""
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 256)
        self.assertIn("-Dasync-api.enabled=false", cmd)
        self.assertIn("--logging.level.root=WARN", cmd)

    def test_command_pins_the_port_and_thread_pool(self):
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 128)
        self.assertIn("--server.port=4845", cmd)
        self.assertIn("--server.tomcat.threads.max=128", cmd)

    def test_command_honours_an_alternate_jdk(self):
        cmd = bm.microcks_cmd("/tmp/app.jar", 4845, 8, java="/opt/jdk21/bin/java")
        self.assertEqual(cmd[0], "/opt/jdk21/bin/java")


class JavaPreflight(unittest.TestCase):
    def test_parses_modern_and_legacy_version_strings(self):
        self.assertEqual(bm.parse_java_major('openjdk version "21.0.5" 2024-10-15'), 21)
        self.assertEqual(bm.parse_java_major('java version "1.8.0_402"'), 8)
        self.assertIsNone(bm.parse_java_major("no version here"))

    def test_requires_21_because_the_jar_is_class_file_65(self):
        self.assertEqual(bm.MIN_JAVA_MAJOR, 21)

    def test_rejects_a_too_old_jdk_with_an_actionable_message(self):
        import subprocess as sp
        real = sp.run
        sp.run = lambda *a, **k: type("R", (), {"stdout": "", "stderr": 'openjdk version "17.0.13"'})()
        try:
            with self.assertRaises(SystemExit) as ctx:
                bm.java_preflight()
            self.assertIn("21", str(ctx.exception))
        finally:
            sp.run = real


class PortsAndUrls(unittest.TestCase):
    def test_offset_is_disjoint_from_the_other_engines(self):
        """rift 0, mb +100, wiremock +200, microcks +300 — the property that stops one engine from
        being measured in place of another when a teardown leaks."""
        self.assertEqual(bm.MICROCKS_OFFSET, 300)

    def test_mock_url_follows_the_microcks_rest_layout(self):
        self.assertEqual(bm.mock_url(4845, "API", "/api/v1/resource1"),
                         "http://localhost:4845/rest/API/1.0.0/api/v1/resource1")

    def test_mock_url_encodes_the_service_name(self):
        self.assertIn("My%20API", bm.mock_url(4845, "My API", "/x"))

    def test_mock_url_preserves_a_query_string(self):
        url = bm.mock_url(4845, "Query", "/query/search?page=100&size=10")
        self.assertTrue(url.endswith("/query/search?page=100&size=10"))


class SpecEnvelope(unittest.TestCase):
    def test_service_name_and_version_drive_the_mock_path(self):
        spec = bm.openapi_spec("API", bench_direct.simple_stubs())
        self.assertEqual(spec["info"]["title"], "API")
        self.assertEqual(spec["info"]["version"], bm.SERVICE_VERSION)

    def test_spec_says_it_is_generated(self):
        """A committed-looking OpenAPI file that someone hand-edits is a fixture drift waiting to
        happen; the description is where that gets said."""
        spec = bm.openapi_spec("API", bench_direct.simple_stubs())
        self.assertIn("bench_microcks.py", spec["info"]["description"])

    def test_spec_is_json_serializable(self):
        json.dumps(bm.openapi_spec("API", bench_direct.api_stubs()))

    def test_multipart_body_is_well_formed(self):
        boundary, body = bm._multipart("file", "x.json", b'{"a":1}')
        self.assertIn(boundary.encode(), body)
        self.assertTrue(body.endswith(f"--{boundary}--\r\n".encode()))
        self.assertIn(b'filename="x.json"', body)
        self.assertIn(b'{"a":1}', body)


class RunSettingsPropagation(unittest.TestCase):
    """The median report must be able to state the settings it was measured with.

    This class exists because the first draft got it wrong in two ways that both survive as a
    plausible-looking report: settings filed under a suffix `--report` never reads (so it printed
    flag *defaults* while claiming to describe the run), and first-rep-wins on disagreement (so a
    median across reps with different heaps described a configuration nothing measured)."""

    def setUp(self):
        import tempfile
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self._real = bm.RESULTS_DIR
        bm.RESULTS_DIR = self.tmp.name
        self.addCleanup(lambda: setattr(bm, "RESULTS_DIR", self._real))

    def _rep(self, n, **settings):
        """A rep's CSV (so `find_reps` sees it) plus its sidecars."""
        with open(os.path.join(self.tmp.name, f"direct_microcks_rep{n}.csv"), "w") as fh:
            fh.write("scenario,connections,mode,rps\napi_last,256,closed,1\n")
        for name, value in settings.items():
            with open(bm.sidecar_path(name, f"_rep{n}"), "w") as fh:
                fh.write(str(value))

    def test_settings_land_where_report_reads_them(self):
        self._rep(1, warmup="10s", threads="256", heap="4g", version="1.14.0")
        self._rep(2, warmup="10s", threads="256", heap="4g", version="1.14.0")
        bm.propagate_run_settings("")
        self.assertEqual(bm.recorded_setting("warmup", bm.median_suffix("")), "10s")
        self.assertEqual(bm.recorded_setting("threads", bm.median_suffix("")), "256")
        self.assertEqual(bm.recorded_setting("heap", bm.median_suffix("")), "4g")
        self.assertEqual(bm.recorded_setting("version", bm.median_suffix("")), "1.14.0")

    def test_median_suffix_is_what_ci_reports_on(self):
        self.assertEqual(bm.median_suffix(""), "_median")

    def test_disagreeing_reps_are_refused_not_averaged(self):
        self._rep(1, heap="4g")
        self._rep(2, heap="8g")
        with self.assertRaises(SystemExit) as ctx:
            bm.propagate_run_settings("")
        self.assertIn("--heap", str(ctx.exception))

    def test_disagreeing_warmup_is_refused(self):
        """The setting the like-for-like claim rests on (#866)."""
        self._rep(1, warmup="10s")
        self._rep(2, warmup="3s")
        with self.assertRaises(SystemExit):
            bm.propagate_run_settings("")

    def test_absent_settings_are_simply_absent(self):
        self._rep(1)
        self.assertEqual(bm.propagate_run_settings(""), {})
        self.assertIsNone(bm.recorded_setting("heap", bm.median_suffix("")))

    def test_finds_every_rep(self):
        self._rep(1, heap="4g")
        self._rep(2, heap="4g")
        self._rep(3, heap="4g")
        self.assertEqual(bm.find_reps(""), [1, 2, 3])

    def test_every_reported_setting_is_propagated(self):
        """A setting the report prints but nobody carries forward renders as a default."""
        self.assertEqual(set(bm.PROPAGATED_SETTINGS),
                         {"warmup", "threads", "heap", "version"})


class ReportRendering(unittest.TestCase):
    """The combiner, driven from CSVs on disk rather than a live engine.

    Worth testing because the report is the artefact people actually read, and its failure modes are
    all silent: a missing engine rendering as a slow one, a stale sweep row blended into the headline
    table, or a ratio computed against a column that was not there."""

    HEADER = ("scenario,connections,mode,rps,p50_ms,p90_ms,p99_ms,p999_ms,avg_ms,"
              "rss_mb_peak,rss_mb_end,reps,rps_spread_pct")

    def setUp(self):
        import tempfile
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self._real = bm.RESULTS_DIR
        bm.RESULTS_DIR = self.tmp.name
        self.addCleanup(lambda: setattr(bm, "RESULTS_DIR", self._real))

    def _csv(self, engine, rows, suffix="_median"):
        path = os.path.join(self.tmp.name, f"direct_{engine}{suffix}.csv")
        with open(path, "w") as fh:
            fh.write(self.HEADER + "\n")
            for scen, conns, mode, rps in rows:
                fh.write(f"{scen},{conns},{mode},{rps},1.0,2.0,3.0,4.0,1.5,,,3,1.2\n")
        return path

    def _read(self, path):
        with open(path) as fh:
            return fh.read()

    def _all_six(self, base, conns=256, mode="closed"):
        return [(n, conns, mode, base + i * 10) for i, n in enumerate(bm.COMPARABLE)]

    def test_renders_with_only_microcks_and_rift(self):
        """The microcks_only dispatch shape: no WireMock column available."""
        self._csv("microcks", self._all_six(5000))
        self._csv("rift", self._all_six(300000))
        text = self._read(bm.report(256, "_median", "20s"))
        self.assertIn("Rift vs Microcks", text)
        for name in bm.COMPARABLE:
            self.assertIn(name, text)

    def test_ratio_is_rift_over_microcks(self):
        self._csv("microcks", [("api_last", 256, "closed", 10000)])
        self._csv("rift", [("api_last", 256, "closed", 300000)])
        text = self._read(bm.report(256, "_median", "20s"))
        self.assertIn("**30.0x**", text)

    def test_absent_rift_does_not_render_as_zero(self):
        """An empty cell must read as "not measured", never as a number."""
        self._csv("microcks", [("api_last", 256, "closed", 10000)])
        text = self._read(bm.report(256, "_median", "20s"))
        self.assertNotIn("0.0x", text)
        self.assertIn("—", text)

    def test_rows_from_another_connection_count_are_not_blended_in(self):
        """A CSV left behind by a sweep holds non-comparable rows; #866's stale-artefact guard."""
        self._csv("microcks", [("api_last", 256, "closed", 10000),
                               ("api_last", 50, "closed", 999999)])
        text = self._read(bm.report(256, "_median", "20s"))
        self.assertIn("10,000", text)
        self.assertNotIn("999,999", text)

    def test_open_loop_rows_are_excluded(self):
        self._csv("microcks", [("api_last", 256, "closed", 10000),
                               ("api_middle", 256, "open@5000", 888888)])
        text = self._read(bm.report(256, "_median", "20s"))
        self.assertNotIn("888,888", text)

    def test_missing_microcks_csv_is_refused_not_rendered_empty(self):
        self._csv("rift", self._all_six(300000))
        with self.assertRaises(SystemExit):
            bm.report(256, "_median", "20s")

    def test_stock_series_renders_its_own_table(self):
        self._csv("microcks", [("api_last", 256, "closed", 10000)])
        self._csv(bm.STOCK_ENGINE, [("api_last", 256, "closed", 5000)])
        text = self._read(bm.report(256, "_median", "20s"))
        self.assertIn("Tuned vs stock defaults", text)
        self.assertIn("-50%", text)   # 5000 vs 10000

    def test_no_stock_series_means_no_stock_table(self):
        """--skip-stock must not leave an empty section implying a measurement."""
        self._csv("microcks", [("api_last", 256, "closed", 10000)])
        text = self._read(bm.report(256, "_median", "20s"))
        self.assertNotIn("Tuned vs stock defaults", text)

    def test_growth_delta_is_simple_health_to_api_last(self):
        self._csv("microcks", [("simple_health", 256, "closed", 10000),
                               ("api_last", 256, "closed", 5000)])
        text = self._read(bm.report(256, "_median", "20s"))
        self.assertIn("-50%", text)

    def test_every_exclusion_reason_reaches_the_report(self):
        """The exclusions are the report's main honesty claim; they must not live only in code."""
        self._csv("microcks", self._all_six(5000))
        text = self._read(bm.report(256, "_median", "20s"))
        for name in bm.UNTRANSLATABLE:
            self.assertIn(f"`{name}`", text)

    def test_report_states_the_asymmetries(self):
        self._csv("microcks", self._all_six(5000))
        text = self._read(bm.report(256, "_median", "20s"))
        for phrase in ("in-memory", "catch-all", "Protocol scope", "spec"):
            self.assertIn(phrase, text)


class PublishWorkflow(unittest.TestCase):
    """The Microcks leg of `benchmark-publish.yml`.

    Same reasoning as the WireMock leg's equivalent: the publication workflow is where a
    like-for-like comparison is won or lost, and the failure modes are silent — a leg that pins its
    own version, or benches a container, still renders a table."""

    WORKFLOW = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..",
                            ".github", "workflows", "benchmark-publish.yml")

    @classmethod
    def setUpClass(cls):
        with open(cls.WORKFLOW) as fh:
            cls.text = fh.read()

    def test_the_leg_exists_and_is_repeated(self):
        self.assertIn("bench_microcks.py --run-all --rep", self.text)

    def test_version_comes_from_the_dispatch_input(self):
        """A hardcoded version in the leg makes the pinned input a lie."""
        self.assertRegex(self.text,
                         r"BENCH_MICROCKS_VERSION:\s*\$\{\{\s*inputs\.microcks_version\s*\}\}")
        self.assertIn('--microcks-version "$BENCH_MICROCKS_VERSION"', self.text)

    def test_default_version_matches_the_module_default(self):
        """Two pins that can disagree are one pin and one bug."""
        self.assertIn(f'default: "{bm.DEFAULT_MICROCKS_VERSION}"', self.text)

    def test_the_jar_is_installed_from_the_image_without_running_it(self):
        """`docker create` materialises the filesystem; `docker run` would start a container. The
        whole point is that the benchmarked process is a native JVM."""
        self.assertIn("docker create", self.text)
        self.assertNotIn('docker run "microcks', self.text)

    def test_the_install_verifies_the_jar_is_a_spring_boot_launcher(self):
        self.assertIn("Start-Class: io.github.microcks.MicrocksApplication", self.text)

    def test_microcks_only_skips_the_legs_it_does_not_need(self):
        """Mountebank and WireMock contribute nothing to a Rift-vs-Microcks table, and re-measuring
        them turns a ~30min dispatch into ~2h."""
        self.assertEqual(self.text.count("!inputs.microcks_only"), 3,
                         "expected the mb, wiremock and sweep legs to be gated")

    def test_microcks_only_measures_rift_itself(self):
        """Rift is the ratio's denominator, so it must come from the SAME dispatch — the one column
        that cannot be cited from a previous run."""
        self.assertIn("bench_direct.py --run-all --engines rift", self.text)
        self.assertIn("BENCH_MICROCKS_ONLY", self.text)

    def test_microcks_only_does_not_read_the_parked_wiremock_artefacts(self):
        """Those only exist when the WireMock leg ran; an unconditional `cp` would fail the job."""
        guarded = self.text.split('if [ "$BENCH_MICROCKS_ONLY" != "true" ]; then', 1)
        self.assertEqual(len(guarded), 2, "the cp-back must be guarded")
        self.assertIn("direct_rift_comparison_median.csv", guarded[1].split("fi", 1)[0])

    def test_the_leg_aggregates_before_reporting(self):
        """Publishing a single rep is what #746 had to retract."""
        agg = self.text.index("bench_microcks.py --aggregate")
        rep = self.text.index("bench_microcks.py --report")
        self.assertLess(agg, rep, "--report must read the aggregated median, not a single rep")

    def test_results_are_uploaded(self):
        self.assertIn("tests/benchmark/results/microcks/*", self.text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
