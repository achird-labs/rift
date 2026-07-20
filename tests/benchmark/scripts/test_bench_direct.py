#!/usr/bin/env python3
"""Gate tests for the pure logic added to bench_direct.py (issue #702).

The live benchmark path (oha + rift under load) is not CI-runnable, so these tests
pin the *logic* the harness relies on: oha command construction (closed vs open-loop),
metric extraction (incl. p999), the connection-sweep parsing, the extended CSV schema
and its round-trip, the recording imposter config, and the journal-depth assertion.

Run: python3 -m unittest test_bench_direct   (from tests/benchmark/scripts)
"""
import io
import json
import os
import sys
import threading
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import bench_direct as bd  # noqa: E402


class ParseConnList(unittest.TestCase):
    def test_parses_comma_list(self):
        self.assertEqual(bd.parse_conn_list("1,10,50,200"), [1, 10, 50, 200])

    def test_single_value(self):
        self.assertEqual(bd.parse_conn_list("50"), [50])

    def test_trims_whitespace(self):
        self.assertEqual(bd.parse_conn_list(" 1 , 200 "), [1, 200])

    def test_rejects_empty(self):
        with self.assertRaises(ValueError):
            bd.parse_conn_list("")

    def test_rejects_non_positive(self):
        with self.assertRaises(ValueError):
            bd.parse_conn_list("0,10")


class BuildOhaCmd(unittest.TestCase):
    def test_closed_loop_has_no_rate_flag(self):
        cmd = bd.build_oha_cmd("http://x/", "GET", None, {}, "20s", 50)
        self.assertNotIn("-q", cmd)
        self.assertIn("-c", cmd)
        self.assertEqual(cmd[cmd.index("-c") + 1], "50")
        self.assertEqual(cmd[-1], "http://x/")

    def test_open_loop_adds_rate_flag(self):
        cmd = bd.build_oha_cmd("http://x/", "GET", None, {}, "20s", 50, rate=1000)
        self.assertIn("-q", cmd)
        self.assertEqual(cmd[cmd.index("-q") + 1], "1000")

    def test_method_headers_body(self):
        cmd = bd.build_oha_cmd(
            "http://x/p", "POST", '{"a":1}', {"Content-Type": "application/json"}, "5s", 10
        )
        self.assertEqual(cmd[cmd.index("-m") + 1], "POST")
        self.assertIn("Content-Type: application/json", cmd)
        self.assertEqual(cmd[cmd.index("-d") + 1], '{"a":1}')

    def test_json_output_format(self):
        cmd = bd.build_oha_cmd("http://x/", "GET", None, {}, "20s", 50)
        self.assertEqual(cmd[cmd.index("--output-format") + 1], "json")


class Metric(unittest.TestCase):
    @staticmethod
    def _oha_json(with_p999=True):
        pct = {"p50": 0.0005, "p90": 0.0008, "p99": 0.0012}
        if with_p999:
            pct["p99.9"] = 0.0031
        return {
            "summary": {"requestsPerSec": 200000.4, "average": 0.0006},
            "latencyPercentiles": pct,
            "statusCodeDistribution": {"200": 100},
        }

    def test_extracts_p999(self):
        m = bd.metric(self._oha_json())
        self.assertEqual(m["p999_ms"], 3.1)
        self.assertEqual(m["p50_ms"], 0.5)
        self.assertEqual(m["p99_ms"], 1.2)
        self.assertEqual(m["rps"], 200000.4)

    def test_missing_p999_is_none(self):
        m = bd.metric(self._oha_json(with_p999=False))
        self.assertIsNone(m["p999_ms"])


class ModeLabel(unittest.TestCase):
    def test_closed(self):
        self.assertEqual(bd.mode_label(None), "closed")

    def test_open(self):
        self.assertEqual(bd.mode_label(1000), "open@1000")


class CsvSchema(unittest.TestCase):
    def test_header_adds_columns_without_dropping_old(self):
        cols = bd.CSV_HEADER.split(",")
        for old in ["scenario", "rps", "p50_ms", "p90_ms", "p99_ms", "avg_ms"]:
            self.assertIn(old, cols)
        for added in ["connections", "mode", "p999_ms"]:
            self.assertIn(added, cols)

    def test_absent_percentile_is_empty_cell_not_none_literal(self):
        m = {"rps": 1000, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
             "p999_ms": None, "avg_ms": 0.6, "codes": {"200": 1}}
        row = bd.csv_row("simple_health", 1, "closed", m)
        self.assertNotIn("None", row)
        parsed = bd.load_rift_csv(io.StringIO(bd.CSV_HEADER + "\n" + row + "\n"))[0]
        self.assertEqual(parsed["p999_ms"], "")

    def test_row_roundtrips_through_loader(self):
        m = {"rps": 200000.4, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
             "p999_ms": 3.1, "avg_ms": 0.6, "codes": {"200": 1}}
        text = bd.CSV_HEADER + "\n" + bd.csv_row("api_middle", 50, "closed", m) + "\n"
        rows = bd.load_rift_csv(io.StringIO(text))
        r = rows[0]
        self.assertEqual(r["scenario"], "api_middle")
        self.assertEqual(int(r["connections"]), 50)
        self.assertEqual(r["mode"], "closed")
        self.assertEqual(float(r["p999_ms"]), 3.1)


class RecordingScenario(unittest.TestCase):
    def test_imposter_records_and_shares_api_stubs(self):
        cfg = bd.recording_imposter_config(0)
        self.assertTrue(cfg["recordRequests"])
        self.assertEqual(cfg["stubs"], bd.api_stubs())

    def test_scenario_is_defined_and_has_body_marker(self):
        name = bd.RECORDING_SCENARIO[0]
        self.assertEqual(name, "recording_on")
        self.assertIn(name, bd.EXPECT_BODY)


class JournalAssertion(unittest.TestCase):
    def test_filled_within_cap_ok(self):
        self.assertTrue(bd.journal_ok(number_of_requests=1_000_000, recorded_len=bd.MAX_RECORDED_REQUESTS))

    def test_rejects_nothing_recorded(self):
        self.assertFalse(bd.journal_ok(number_of_requests=0, recorded_len=0))

    def test_rejects_over_cap(self):
        self.assertFalse(bd.journal_ok(number_of_requests=1, recorded_len=bd.MAX_RECORDED_REQUESTS + 1))


class ResolveRunMode(unittest.TestCase):
    def test_default_both_engines_single_point_closed(self):
        engines, conn_list, rate, recording = bd.resolve_run_mode("rift,mb", None, None, 50)
        self.assertEqual(engines, ["rift", "mb"])
        self.assertEqual(conn_list, [50])
        self.assertIsNone(rate)
        self.assertFalse(recording)

    def test_sweep_forces_rift_only_and_recording(self):
        engines, conn_list, rate, recording = bd.resolve_run_mode("rift,mb", "1,10,50,200", None, 50)
        self.assertEqual(engines, ["rift"])
        self.assertEqual(conn_list, [1, 10, 50, 200])
        self.assertTrue(recording)

    def test_open_loop_forces_rift_only_and_sets_rate(self):
        engines, conn_list, rate, recording = bd.resolve_run_mode("rift", None, 1000, 200)
        self.assertEqual(engines, ["rift"])
        self.assertEqual(conn_list, [200])
        self.assertEqual(rate, 1000)
        self.assertTrue(recording)

    def test_rejects_unknown_engine(self):
        with self.assertRaises(ValueError):
            bd.resolve_run_mode("rtift", None, None, 50)

    def test_rejects_empty_engines(self):
        with self.assertRaises(ValueError):
            bd.resolve_run_mode("", None, None, 50)


class RiftOnlyReport(unittest.TestCase):
    """AC1: the sweep report is a scenario × connection matrix of RPS, p50, p99, p999."""

    def _write_and_report(self, tmp, rows, conn_list, rate, recording):
        orig = bd.RESULTS_DIR
        bd.RESULTS_DIR = tmp
        try:
            bd.write_rift_csv(os.path.join(tmp, "direct_rift.csv"), rows)
            bd.rift_only_report("0.1.0", "20s", conn_list, rate, recording)
            with open(os.path.join(tmp, "DIRECT_RIFT_SWEEP_REPORT.md")) as f:
                return f.read()
        finally:
            bd.RESULTS_DIR = orig

    @staticmethod
    def _m(rps, p999=3.1):
        return {"rps": rps, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
                "p999_ms": p999, "avg_ms": 0.6, "codes": {"200": 1}}

    def test_matrix_has_all_four_percentile_sections_and_marks_recording(self):
        import tempfile
        rows = []
        for c in (50, 200):
            for name, *_ in bd.SCENARIOS:
                rows.append((name, c, "closed", self._m(200000 + c)))
            rows.append(("recording_on", c, "closed", self._m(150000 + c)))
        out = self._write_and_report(tempfile.mkdtemp(), rows, [50, 200], None, True)
        # AC1: RPS + p50 + p99 + p999 matrices all present
        for title in ["Throughput", "Latency p50", "Latency p99", "Tail latency p999"]:
            self.assertIn(title, out)
        # both swept connection columns are present
        self.assertIn("c=50", out)
        self.assertIn("c=200", out)
        # AC2: recording row clearly marked
        self.assertIn("recording_on **(recording)**", out)

    def test_absent_percentile_renders_na_not_none(self):
        import tempfile
        rows = [("simple_health", 1, "closed", self._m(1000, p999=None))]
        out = self._write_and_report(tempfile.mkdtemp(), rows, [1], None, False)
        self.assertIn("n/a", out)
        self.assertNotIn("None", out)


class DefaultRunUnchanged(unittest.TestCase):
    """AC4: the default MB-comparison scenario set must not change."""

    def test_scenario_names_and_count_stable(self):
        names = [s[0] for s in bd.SCENARIOS]
        self.assertEqual(len(names), 13)
        self.assertNotIn("recording_on", names)  # recording_on is additive, not in the MB set
        self.assertEqual(names[0], "simple_health")
        self.assertEqual(names[-1], "query_last")


class ComparisonReps(unittest.TestCase):
    """The Rift-vs-Mountebank table is the number people quote, so it must be replicable.
    It previously could not be: report() read unsuffixed artefacts, so --rep was refused for
    rift+mb and the headline table was always a single unreplicated sample per engine."""

    @staticmethod
    def _rows(rps):
        # Every MB-set scenario: report() renders the full table and raises on a missing one,
        # which is correct — a partial comparison should fail loudly, not render half a table.
        return [(name, 50, "closed",
                 {"rps": rps, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
                  "p999_ms": 3.1, "avg_ms": 0.6, "codes": {"200": 1}})
                for name, *_ in bd.SCENARIOS]

    def test_report_raises_on_an_incomplete_comparison(self):
        # Guards the behaviour the fixture above accommodates: if a scenario is missing from one
        # engine's results, the table must not quietly render without it.
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                bd.write_rift_csv(os.path.join(tmp, "direct_rift_p.csv"), self._rows(100.0))
                partial = [r for r in self._rows(10.0) if r[0] != "api_first"]
                bd.write_rift_csv(os.path.join(tmp, "direct_mb_p.csv"), partial)
                with self.assertRaises(KeyError):
                    bd.report("0.1.0", "2.9.1", "20s", 50, "_p")
            finally:
                bd.RESULTS_DIR = orig

    def test_report_is_suffix_aware(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                bd.write_rift_csv(os.path.join(tmp, "direct_rift_x.csv"), self._rows(100.0))
                bd.write_rift_csv(os.path.join(tmp, "direct_mb_x.csv"), self._rows(10.0))
                bd.report("0.1.0", "2.9.1", "20s", 50, "_x")
                self.assertTrue(os.path.exists(os.path.join(tmp, "DIRECT_BENCHMARK_REPORT_x.md")))
            finally:
                bd.RESULTS_DIR = orig

    def test_comparison_medians_both_engines_and_shows_spread(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                for rep, (r, m) in enumerate([(100.0, 10.0), (300.0, 12.0), (200.0, 11.0)], 1):
                    bd.write_rift_csv(os.path.join(tmp, f"direct_rift_c_rep{rep}.csv"), self._rows(r))
                    bd.write_rift_csv(os.path.join(tmp, f"direct_mb_c_rep{rep}.csv"), self._rows(m))
                out = bd.aggregate_comparison_reps("_c", "0.1.0", "2.9.1", 50)
                text = open(out).read()
                self.assertIn("200", text)     # rift median, not 100 or 300
                self.assertIn("11", text)      # mb median
                self.assertIn("spread", text.lower())
            finally:
                bd.RESULTS_DIR = orig

    def test_unequal_rep_counts_are_refused(self):
        # A table comparing 3 Rift reps against 1 Mountebank rep favours whichever engine got
        # more samples, and nothing in the output would say so.
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                for rep in (1, 2, 3):
                    bd.write_rift_csv(os.path.join(tmp, f"direct_rift_u_rep{rep}.csv"), self._rows(100.0))
                bd.write_rift_csv(os.path.join(tmp, "direct_mb_u_rep1.csv"), self._rows(10.0))
                with self.assertRaises(SystemExit) as cm:
                    bd.aggregate_comparison_reps("_u", "0.1.0", "2.9.1", 50)
                self.assertIn("rep-count mismatch", str(cm.exception))
            finally:
                bd.RESULTS_DIR = orig

    def test_missing_engine_reps_is_an_error(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                with self.assertRaises(SystemExit):
                    bd.aggregate_comparison_reps("_nothing", "0.1.0", "2.9.1", 50)
            finally:
                bd.RESULTS_DIR = orig


class DimensionSetIsAdditive(unittest.TestCase):
    """The MB-comparison set is a stability contract; dimension scenarios must never leak into it,
    or previously published Mountebank numbers stop being comparable."""

    def test_dimension_scenarios_are_not_in_the_mb_set(self):
        mb = {s[0] for s in bd.SCENARIOS}
        for name, *_ in bd.DIMENSION_SCENARIOS:
            self.assertNotIn(name, mb, f"{name} leaked into the MB-comparison set")

    def test_dimension_scenarios_use_their_own_ports(self):
        mb_ports = {s[1] for s in bd.SCENARIOS}
        for name, port, *_ in bd.DIMENSION_SCENARIOS:
            self.assertNotIn(port, mb_ports, f"{name} shares a port with the MB set")

    def test_report_lists_every_scenario_the_sweep_measures(self):
        # A scenario that is benched but missing from `scen_order` is measured and then silently
        # dropped from the report — cost paid, number invisible.
        import tempfile
        rows = [(name, 256, "closed",
                 {"rps": 1.0, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
                  "p999_ms": 3.1, "avg_ms": 0.6, "codes": {"200": 1}})
                for name, *_ in bd.SCENARIOS + bd.DIMENSION_SCENARIOS + [bd.RECORDING_SCENARIO]]
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                bd.write_rift_csv(os.path.join(tmp, "direct_rift.csv"), rows)
                bd.rift_only_report("0.1.0", "12s", [256], None, True)
                with open(os.path.join(tmp, "DIRECT_RIFT_SWEEP_REPORT.md")) as f:
                    text = f.read()
            finally:
                bd.RESULTS_DIR = orig
        for name, *_ in bd.DIMENSION_SCENARIOS:
            self.assertIn(name, text, f"{name} was benched but is absent from the report")


class AllocatorBuildArgs(unittest.TestCase):
    """Issue #717: each allocator maps to cargo flags that swap ONLY the allocator, keeping the
    functional feature set (redis-backend, javascript) identical so the comparison is fair."""

    def test_mimalloc_is_the_default_build(self):
        self.assertEqual(bd.allocator_build_args("mimalloc"), [])

    def test_jemalloc_swaps_allocator_only(self):
        self.assertEqual(
            bd.allocator_build_args("jemalloc"),
            ["--no-default-features", "--features", "redis-backend,javascript,jemalloc"],
        )

    def test_system_drops_the_allocator_only(self):
        self.assertEqual(
            bd.allocator_build_args("system"),
            ["--no-default-features", "--features", "redis-backend,javascript"],
        )

    def test_rejects_unknown_allocator(self):
        with self.assertRaises(ValueError):
            bd.allocator_build_args("tcmalloc")


class ResolveRiftBin(unittest.TestCase):
    """Issue #717: --allocator builds its own binary into a per-allocator target dir unless the
    caller supplied an explicit --rift-bin (then it is trusted verbatim, no build)."""

    def test_no_allocator_uses_default_path(self):
        path, build = bd.resolve_rift_bin(None, None)
        self.assertEqual(path, bd.DEFAULT_RIFT_BIN)
        self.assertFalse(build)

    def test_explicit_bin_is_used_verbatim_even_with_allocator(self):
        path, build = bd.resolve_rift_bin("/tmp/custom-rift", "jemalloc")
        self.assertEqual(path, "/tmp/custom-rift")
        self.assertFalse(build)

    def test_allocator_without_bin_builds_into_per_allocator_target(self):
        path, build = bd.resolve_rift_bin(None, "jemalloc")
        self.assertIn("alloc-jemalloc", path)
        self.assertTrue(build)


class ParseRssKb(unittest.TestCase):
    """Issue #717: `ps -o rss=` output (KB) parsing; absent/garbage reads as None, never 0."""

    def test_parses_ps_output(self):
        self.assertEqual(bd.parse_rss_kb(" 123456\n"), 123456)

    def test_empty_is_none(self):
        self.assertIsNone(bd.parse_rss_kb(""))

    def test_garbage_is_none(self):
        self.assertIsNone(bd.parse_rss_kb("abc"))


class CsvRssColumns(unittest.TestCase):
    """Issue #717: the CSV grows rss_mb_peak/rss_mb_end; rows without RSS keep empty cells so
    DictReader consumers and pre-#717 rows stay compatible."""

    _BASE = {"rps": 1000, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
             "p999_ms": 3.1, "avg_ms": 0.6, "codes": {"200": 1}}

    def test_header_gains_rss_columns(self):
        cols = bd.CSV_HEADER.split(",")
        self.assertIn("rss_mb_peak", cols)
        self.assertIn("rss_mb_end", cols)

    def test_rows_without_rss_have_empty_cells(self):
        row = bd.csv_row("api_middle", 50, "closed", dict(self._BASE))
        parsed = bd.load_rift_csv(io.StringIO(bd.CSV_HEADER + "\n" + row + "\n"))[0]
        self.assertEqual(parsed["rss_mb_peak"], "")
        self.assertEqual(parsed["rss_mb_end"], "")

    def test_rss_round_trips(self):
        m = dict(self._BASE, rss_mb_peak=61.2, rss_mb_end=58.9)
        row = bd.csv_row("api_middle", 50, "closed", m)
        parsed = bd.load_rift_csv(io.StringIO(bd.CSV_HEADER + "\n" + row + "\n"))[0]
        self.assertEqual(float(parsed["rss_mb_peak"]), 61.2)
        self.assertEqual(float(parsed["rss_mb_end"]), 58.9)


class AllocatorMarker(unittest.TestCase):
    """Issue #717: the results label must come from the binary's own startup self-report
    (`Global allocator: <name>`), so a wrong --rift-bin can never silently mislabel a sweep."""

    def test_extracts_name_from_log(self):
        log = ("2026-07-17T00:00:00Z  INFO rift: Starting Rift on port 3525\n"
               "2026-07-17T00:00:00Z  INFO rift: Global allocator: jemalloc\n")
        self.assertEqual(bd.extract_allocator_marker(log), "jemalloc")

    def test_absent_marker_is_none(self):
        self.assertIsNone(bd.extract_allocator_marker("INFO rift: Starting Rift on port 3525\n"))

    def test_empty_log_is_none(self):
        self.assertIsNone(bd.extract_allocator_marker(""))




class RuntimeLaunchArgs(unittest.TestCase):
    """Issue #746: --runtime pass-through builds the engine flag; unknown modes are hard errors."""

    def test_none_adds_nothing(self):
        self.assertEqual(bd.runtime_launch_args(None), [])

    def test_work_stealing_is_explicit(self):
        self.assertEqual(bd.runtime_launch_args("work-stealing"), ["--runtime", "work-stealing"])

    def test_per_core_passes_through(self):
        self.assertEqual(bd.runtime_launch_args("per-core"), ["--runtime", "per-core"])

    def test_rejects_unknown_mode(self):
        with self.assertRaises(ValueError):
            bd.runtime_launch_args("thread-per-request")


class TopologyMarker(unittest.TestCase):
    """Issue #746: results are labeled by the binary's own `Runtime topology:` self-report —
    on macOS a requested per-core falls back to work-stealing, and the probe must refuse to
    run a sweep that would be mislabeled."""

    def test_extracts_marker(self):
        log = "INFO rift: Runtime topology: per-core x8\n"
        self.assertEqual(bd.extract_topology_marker(log), "per-core x8")

    def test_absent_is_none(self):
        self.assertIsNone(bd.extract_topology_marker("INFO rift: Starting Rift\n"))

    def test_match_exact_and_prefixed(self):
        self.assertTrue(bd.topology_matches("work-stealing", "work-stealing"))
        self.assertTrue(bd.topology_matches("per-core x8", "per-core"))
        self.assertFalse(bd.topology_matches("work-stealing", "per-core"))
        self.assertFalse(bd.topology_matches("per-core x8", "work-stealing"))

    def test_core_budget_must_equal_reported_worker_count(self):
        # The core-count axis is only real if taskset reached available_parallelism().
        self.assertTrue(bd.topology_matches("per-core x4", "per-core", expected_workers=4))
        self.assertFalse(bd.topology_matches("per-core x16", "per-core", expected_workers=4))
        self.assertFalse(bd.topology_matches("work-stealing", "per-core", expected_workers=4))

    def test_work_stealing_reports_no_count_to_check(self):
        self.assertTrue(bd.topology_matches("work-stealing", "work-stealing", expected_workers=4))


class ResultSuffix(unittest.TestCase):
    """Issue #746: allocator and runtime dimensions compose into one artefact suffix so no
    combination overwrites another's results."""

    def test_neither(self):
        self.assertEqual(bd.result_suffix(None, None), "")

    def test_allocator_only(self):
        self.assertEqual(bd.result_suffix("jemalloc", None), "_jemalloc")

    def test_runtime_only(self):
        self.assertEqual(bd.result_suffix(None, "per-core"), "_per-core")

    def test_both_compose(self):
        self.assertEqual(bd.result_suffix("jemalloc", "per-core"), "_jemalloc_per-core")

    def test_core_count_is_its_own_dimension(self):
        self.assertEqual(bd.result_suffix(None, "per-core", 8), "_per-core_cores8")
        self.assertEqual(bd.result_suffix(None, "work-stealing", 8), "_work-stealing_cores8")

    def test_all_three_compose(self):
        self.assertEqual(bd.result_suffix("mimalloc", "per-core", 4),
                         "_mimalloc_per-core_cores4")

    def test_rep_is_its_own_dimension(self):
        # Issue #773: a repetition must be part of the artefact name, not something the caller
        # bolts on after the fact — otherwise every rep overwrites the last and the canonical-
        # looking file holds one unreplicated sample.
        self.assertEqual(bd.result_suffix(None, "per-core", 8, rep=2), "_per-core_cores8_rep2")
        self.assertEqual(bd.result_suffix(None, None, None, rep=1), "_rep1")

    def test_all_four_compose(self):
        self.assertEqual(bd.result_suffix("mimalloc", "per-core", 4, rep=3),
                         "_mimalloc_per-core_cores4_rep3")

    def test_quamina_and_stub_count_are_their_own_dimensions(self):
        # Issue #779: the A/B variant and the stub count are what the measurement varies, so they
        # must name the artefact — otherwise the two halves of the bake-off overwrite each other.
        self.assertEqual(bd.result_suffix(None, None, None, quamina="on"), "_quaminaon")
        self.assertEqual(bd.result_suffix(None, None, None, stub_count=1000), "_stubs1000")
        self.assertEqual(bd.result_suffix(None, None, None, quamina="off", stub_count=100),
                         "_quaminaoff_stubs100")

    def test_rep_stays_outermost(self):
        # Every other dimension names a variant; rep names a sample OF one, so it sorts last.
        self.assertEqual(
            bd.result_suffix(None, "per-core", 8, rep=2, quamina="on", stub_count=1000),
            "_per-core_cores8_quaminaon_stubs1000_rep2")

    def test_new_dimensions_absent_reproduces_the_old_names(self):
        self.assertEqual(bd.result_suffix("jemalloc", "per-core", 4, rep=1), "_jemalloc_per-core_cores4_rep1")

    def test_rep_absent_reproduces_the_old_names(self):
        # The pre-#773 contract must be byte-identical when no rep is given, so artefacts from
        # earlier sweeps stay comparable.
        self.assertEqual(bd.result_suffix("jemalloc", "per-core", 4, rep=None),
                         "_jemalloc_per-core_cores4")


class MedianOfReps(unittest.TestCase):
    """Issue #773: the decision artefact must be an explicit median across reps with the per-rep
    spread visible — a degraded rep should be observable, not silently averaged in (or, worse,
    silently the only sample)."""

    @staticmethod
    def _rows(rps_by_scenario, conns=256):
        return [
            {"scenario": s, "connections": str(conns), "mode": "closed", "rps": str(v),
             "p50_ms": "1.0", "p90_ms": "2.0", "p99_ms": "3.0", "p999_ms": "4.0",
             "avg_ms": "1.5", "rss_mb_peak": "50.0", "rss_mb_end": "40.0"}
            for s, v in rps_by_scenario.items()
        ]

    def test_median_of_three_reps_picks_the_middle(self):
        reps = [self._rows({"a": 100.0}), self._rows({"a": 300.0}), self._rows({"a": 200.0})]
        agg = bd.aggregate_reps(reps)
        self.assertEqual(agg[("a", 256, "closed")]["rps"], 200.0)

    def test_median_of_even_count_averages_the_middle_two(self):
        reps = [self._rows({"a": 100.0}), self._rows({"a": 200.0})]
        agg = bd.aggregate_reps(reps)
        self.assertEqual(agg[("a", 256, "closed")]["rps"], 150.0)

    def test_spread_exposes_a_degraded_rep(self):
        # The #746 case: rep3 ran ~20% low on a degraded runner. The aggregate must SAY so.
        reps = [self._rows({"a": 5510000.0}), self._rows({"a": 5620000.0}),
                self._rows({"a": 4350000.0})]
        agg = bd.aggregate_reps(reps)
        cell = agg[("a", 256, "closed")]
        self.assertEqual(cell["rps"], 5510000.0)
        self.assertGreater(cell["rps_spread_pct"], 20.0)
        self.assertEqual(cell["reps"], 3)

    def test_spread_is_zero_for_identical_reps(self):
        reps = [self._rows({"a": 100.0}), self._rows({"a": 100.0})]
        self.assertEqual(bd.aggregate_reps(reps)[("a", 256, "closed")]["rps_spread_pct"], 0.0)

    def test_latency_percentiles_are_aggregated_too(self):
        reps = [self._rows({"a": 100.0}), self._rows({"a": 100.0}), self._rows({"a": 100.0})]
        cell = bd.aggregate_reps(reps)[("a", 256, "closed")]
        for field in ("p50_ms", "p99_ms", "p999_ms"):
            self.assertIn(field, cell)

    def test_rejects_empty_rep_set(self):
        with self.assertRaises(ValueError):
            bd.aggregate_reps([])

    def test_single_rep_is_its_own_median_with_zero_spread(self):
        cell = bd.aggregate_reps([self._rows({"a": 42.0})])[("a", 256, "closed")]
        self.assertEqual(cell["rps"], 42.0)
        self.assertEqual(cell["rps_spread_pct"], 0.0)
        self.assertEqual(cell["reps"], 1)

    def test_percentile_absent_in_only_some_reps_medians_the_present_ones(self):
        # oha can omit p99.9 on a sparse point in one rep but not another. The median must come
        # from what was actually measured, never treating the gap as a zero.
        a, b = self._rows({"x": 100.0}), self._rows({"x": 100.0})
        a[0]["p999_ms"] = ""
        b[0]["p999_ms"] = "9.0"
        cell = bd.aggregate_reps([a, b])[("x", 256, "closed")]
        self.assertEqual(cell["p999_ms"], 9.0)

    def test_absent_percentile_does_not_crash_the_median(self):
        # oha omits p99.9 when a point has too few samples; the CSV cell is empty, not "None".
        reps = self._rows({"a": 100.0}), self._rows({"a": 200.0})
        for r in reps:
            r[0]["p999_ms"] = ""
        cell = bd.aggregate_reps(list(reps))[("a", 256, "closed")]
        self.assertEqual(cell["p999_ms"], "")


class RepArtefacts(unittest.TestCase):
    """Issue #773: the whole point is that a repped run leaves no file claiming to be more than
    the one sample it is."""

    def test_discovers_rep_files_for_a_base_suffix(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                for rep in (1, 2, 3):
                    open(os.path.join(tmp, f"direct_rift_per-core_cores8_rep{rep}.csv"), "w").close()
                # decoys that must NOT be collected: a different variant, and a stale unsuffixed file
                open(os.path.join(tmp, "direct_rift_work-stealing_cores8_rep1.csv"), "w").close()
                open(os.path.join(tmp, "direct_rift_per-core_cores8.csv"), "w").close()
                found = bd.find_rep_files("_per-core_cores8")
                self.assertEqual(len(found), 3, f"expected exactly the 3 rep files, got {found}")
                self.assertTrue(all("_rep" in os.path.basename(f) for f in found))
                self.assertTrue(all("work-stealing" not in f for f in found))
            finally:
                bd.RESULTS_DIR = orig

    def test_missing_reps_is_an_error_not_an_empty_aggregate(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                with self.assertRaises(SystemExit):
                    bd.aggregate_reps_to_report("_nonexistent", "0.1.0")
            finally:
                bd.RESULTS_DIR = orig

    def test_repped_writes_produce_only_suffixed_artefacts(self):
        # THE acceptance criterion: a repped run must leave no unsuffixed file. Asserting on
        # `result_suffix`'s string is not enough — a back-compat shim that ALSO wrote the
        # unsuffixed name would satisfy that and reintroduce the whole bug.
        import tempfile
        rows = [("simple_health", 256, "closed",
                 {"rps": 1.0, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
                  "p999_ms": 3.1, "avg_ms": 0.6, "codes": {"200": 1}})]
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                for rep in (1, 2, 3):
                    bd.write_results_csv("rift", bd.result_suffix(None, "per-core", 8, rep=rep), rows)
                produced = sorted(os.listdir(tmp))
                self.assertEqual(produced, [
                    "direct_rift_per-core_cores8_rep1.csv",
                    "direct_rift_per-core_cores8_rep2.csv",
                    "direct_rift_per-core_cores8_rep3.csv",
                ], "a repped run must write exactly one file per rep and no unsuffixed artefact")
                self.assertNotIn("direct_rift_per-core_cores8.csv", produced)
            finally:
                bd.RESULTS_DIR = orig

    def test_unrepped_write_keeps_the_unsuffixed_name(self):
        import tempfile
        rows = [("simple_health", 256, "closed",
                 {"rps": 1.0, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
                  "p999_ms": 3.1, "avg_ms": 0.6, "codes": {"200": 1}})]
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                bd.write_results_csv("rift", bd.result_suffix(None, "per-core", 8), rows)
                self.assertEqual(os.listdir(tmp), ["direct_rift_per-core_cores8.csv"])
            finally:
                bd.RESULTS_DIR = orig

    def test_partial_reps_are_an_error_not_a_quietly_thinner_median(self):
        # A point missing from one rep (crashed run, changed --sweep-connections, truncated CSV)
        # must fail loudly — a complete-looking report resting on 2 of 3 samples is the silence
        # #773 exists to remove.
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                full = [("a", 256, "closed", self._metric(100.0)),
                        ("b", 256, "closed", self._metric(200.0))]
                partial = [("a", 256, "closed", self._metric(110.0))]   # 'b' missing here
                bd.write_rift_csv(os.path.join(tmp, "direct_rift_p_rep1.csv"), full)
                bd.write_rift_csv(os.path.join(tmp, "direct_rift_p_rep2.csv"), partial)
                with self.assertRaises(SystemExit) as cm:
                    bd.aggregate_reps_to_report("_p", "0.1.0")
                self.assertIn("incomplete repetitions", str(cm.exception))
                self.assertIn("b@c=256", str(cm.exception))
            finally:
                bd.RESULTS_DIR = orig

    @staticmethod
    def _metric(rps):
        return {"rps": rps, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
                "p999_ms": 3.1, "avg_ms": 0.6, "codes": {"200": 1}}

    def test_aggregate_report_states_rep_count_and_spread(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            orig = bd.RESULTS_DIR
            bd.RESULTS_DIR = tmp
            try:
                for rep, rps in ((1, 5510000.0), (2, 5620000.0), (3, 4350000.0)):
                    rows = [("simple_health", 256, "closed",
                             {"rps": rps, "p50_ms": 0.5, "p90_ms": 0.8, "p99_ms": 1.2,
                              "p999_ms": 3.1, "avg_ms": 0.6, "codes": {"200": 1}})]
                    bd.write_rift_csv(os.path.join(tmp, f"direct_rift_x_rep{rep}.csv"), rows)
                out = bd.aggregate_reps_to_report("_x", "0.1.0")
                text = open(out).read()
                self.assertIn("spread", text.lower())
                self.assertIn("3", text)          # rep count is stated
                self.assertIn("5,510,000", text)  # the median, not rep3's degraded 4.35M
            finally:
                bd.RESULTS_DIR = orig


class QuaminaVariant(unittest.TestCase):
    """Issue #779: the two variants match IDENTICALLY by design — the dimension is a pure
    prefilter — so a mislabeled build produces no visible symptom anywhere else in the results.
    That makes the build args and the self-report check the only things standing between the
    bake-off and a silently meaningless number."""

    def test_on_uses_default_features(self):
        self.assertEqual(bd.quamina_build_args("on"), [])

    def test_off_drops_only_the_dimension(self):
        args = bd.quamina_build_args("off")
        self.assertIn("--no-default-features", args)
        feats = args[args.index("--features") + 1]
        # Everything else the default build has must survive, or the two variants differ by more
        # than the one thing under test.
        for keep in ("redis-backend", "javascript", "mimalloc"):
            self.assertIn(keep, feats)
        self.assertNotIn("quamina", feats)

    def test_rejects_unknown_variant(self):
        with self.assertRaises(ValueError):
            bd.quamina_build_args("maybe")

    def test_marker_extraction_and_matching(self):
        log = "INFO rift_http_proxy: Matching dimensions: body-field(quamina)=on\n"
        self.assertEqual(bd.extract_quamina_marker(log), "body-field(quamina)=on")
        self.assertTrue(bd.quamina_marker_matches("body-field(quamina)=on", "on"))
        self.assertFalse(bd.quamina_marker_matches("body-field(quamina)=off", "on"))
        self.assertFalse(bd.quamina_marker_matches("body-field(quamina)=on", "off"))

    def test_absent_marker_is_none(self):
        self.assertIsNone(bd.extract_quamina_marker("INFO rift: Starting Rift on port 2525\n"))

    def test_variant_binaries_do_not_share_a_target_dir(self):
        self.assertNotEqual(bd.quamina_bin_path("on"), bd.quamina_bin_path("off"))

    def test_explicit_rift_bin_still_wins(self):
        path, needs_build = bd.resolve_rift_bin("/tmp/custom-rift", None, "on")
        self.assertEqual(path, "/tmp/custom-rift")
        self.assertFalse(needs_build)

    def test_quamina_selects_its_variant_binary(self):
        path, needs_build = bd.resolve_rift_bin(None, None, "off")
        self.assertIn("quamina-off", path)
        self.assertTrue(needs_build)


class ScenarioReachability(unittest.TestCase):
    """Every scenario must address a stub that actually exists.

    #784 shipped a scenario pointing at `/json/equals/25` against a 10-stub imposter: the request
    fell through to the no-match default and the run aborted after a full release build. The
    runtime body assertion caught it, but only in CI. This is the same check, statically, for
    every scenario — so adding a scenario whose target nothing serves fails in milliseconds."""

    @staticmethod
    def _predicate_serves(pred, method, path):
        """Does this predicate plausibly match? Covers the operator forms the suite uses."""
        import re as _re
        for op, spec in pred.items():
            if op in ("startsWith", "contains", "matches", "equals", "deepEquals", "exists"):
                want_path = spec.get("path") if isinstance(spec, dict) else None
                want_method = spec.get("method") if isinstance(spec, dict) else None
                if want_method and want_method != method:
                    return False
                if want_path is None:
                    continue
                bare = path.split("?", 1)[0]
                if op in ("equals", "deepEquals") and bare != want_path:
                    return False
                if op == "startsWith" and not bare.startswith(want_path):
                    return False
                if op == "contains" and want_path not in bare:
                    return False
                if op == "matches" and not _re.search(want_path, bare):
                    return False
            elif op in ("and", "or"):
                results = [ScenarioReachability._predicate_serves(p, method, path) for p in spec]
                if (op == "and" and not all(results)) or (op == "or" and not any(results)):
                    return False
        return True

    def test_every_scenario_targets_a_stub_that_exists(self):
        by_port = {port: stubs for port, _, stubs in bd.IMPOSTERS}
        unreachable = []
        for name, port, method, path, _body, _headers in bd.SCENARIOS + bd.DIMENSION_SCENARIOS:
            if name == "no_match":
                continue          # deliberately matches nothing — that IS the scenario
            stubs = by_port.get(port)
            self.assertIsNotNone(stubs, f"{name}: no imposter on port {port}")
            if not any(all(self._predicate_serves(p, method, path) for p in st.get("predicates", []))
                       for st in stubs):
                unreachable.append(f"{name} -> {method} {path} (port {port})")
        self.assertEqual(unreachable, [], "scenarios whose target no stub serves: " + str(unreachable))

    def test_the_no_match_scenario_really_matches_nothing(self):
        # Its whole point is the fall-through path; if a stub started serving it, the scenario
        # would silently stop measuring what it claims to.
        scen = {s[0]: s for s in bd.SCENARIOS}["no_match"]
        stubs = {p: s for p, _, s in bd.IMPOSTERS}[scen[1]]
        served = any(all(self._predicate_serves(p, scen[2], scen[3]) for p in st.get("predicates", []))
                     for st in stubs)
        self.assertFalse(served, "no_match now matches a stub — it no longer measures fall-through")

    def test_every_scenario_has_a_body_marker(self):
        for name, *_ in bd.SCENARIOS + bd.DIMENSION_SCENARIOS:
            self.assertIn(name, bd.EXPECT_BODY,
                          f"{name} has no EXPECT_BODY marker, so a fall-through would go unnoticed")

    def test_imposter_ports_are_unique(self):
        ports = [p for p, _, _ in bd.IMPOSTERS]
        self.assertEqual(len(ports), len(set(ports)), f"duplicate imposter ports: {ports}")

    def test_no_imposter_collides_with_a_reserved_port(self):
        """The check that was missing.

        The previous version compared imposter ports only against each other, so it happily
        allowed DeepEquals onto 4556 — which is RECORDING_PORT. The recording imposter is created
        last, hit `400 Port 4556 is already in use`, and the resulting error was then masked by
        cleanup (see `free_ports`), costing four CI runs to find. Every port the harness reserves
        belongs in this check, not just the imposter list."""
        reserved = {
            bd.RECORDING_PORT: "RECORDING_PORT",
            bd.admin_port_for(0): "rift admin",
            bd.admin_port_for(100): "mountebank admin",
            9090: "metrics",
            bd.ALLOC_PROBE_PORT: "allocator probe",
            bd.TOPOLOGY_PROBE_PORT: "topology probe",
            bd.QUAMINA_PROBE_PORT: "quamina probe",
        }
        for port, name, _ in bd.IMPOSTERS:
            self.assertNotIn(port, reserved,
                             f"imposter {name} is on port {port}, reserved for "
                             f"{reserved.get(port)}")

    def test_the_mb_offset_does_not_alias_a_rift_port(self):
        """Mountebank runs at +100. If that offset ever mapped one engine's port onto another's,
        the two engines would fight over a listener mid-run."""
        rift_ports = {p for p, _, _ in bd.IMPOSTERS} | {bd.RECORDING_PORT, bd.admin_port_for(0)}
        mb_ports = {p + 100 for p, _, _ in bd.IMPOSTERS} | {
            bd.RECORDING_PORT + 100, bd.admin_port_for(100)}
        self.assertEqual(rift_ports & mb_ports, set(),
                         f"offset collision between engines: {rift_ports & mb_ports}")

    def test_free_ports_only_targets_listeners(self):
        """A bare `lsof -ti tcp:PORT` matches client sockets too, so the harness could SIGKILL
        itself during cleanup and destroy the error it was reporting."""
        import inspect
        src = inspect.getsource(bd.free_ports)
        self.assertIn("-sTCP:LISTEN", src)
        self.assertIn("os.getpid()", src)


class NewDimensionCoverage(unittest.TestCase):
    """The scenarios added for optimizations that previously had none."""

    def test_deepequals_dimension_is_exercised(self):
        # Issue #740's structural-hash index. Before this, `deepEquals` appeared nowhere.
        stubs = {n: s for _, n, s in bd.IMPOSTERS}["DeepEquals"]
        self.assertTrue(all("deepEquals" in st["predicates"][0] for st in stubs))
        self.assertIn("deepequals_body", {s[0] for s in bd.DIMENSION_SCENARIOS})

    def test_deepequals_bodies_are_objects_not_scalars(self):
        # The index only applies to a JSON object/array body; a scalar falls back to full
        # comparison, so a scalar-bodied scenario would measure the unindexed path by accident.
        for st in {n: s for _, n, s in bd.IMPOSTERS}["DeepEquals"]:
            self.assertIsInstance(st["predicates"][0]["deepEquals"]["body"], dict)

    def test_literal_dimension_has_both_anchored_and_unanchored(self):
        # #732 indexes startsWith (anchored) and contains (unanchored) differently.
        ops = {op for st in {n: s for _, n, s in bd.IMPOSTERS}["Literal"]
               for op in st["predicates"][0]}
        self.assertEqual(ops, {"startsWith", "contains"})

    def test_body_field_stubs_share_one_path_so_the_body_is_the_discriminator(self):
        # THE lesson from run 29738479074: json_body_stubs gives every stub a unique path, so the
        # path dimension prunes to one candidate before the body is consulted and the quamina
        # automaton measures pure overhead (-8%, flat with N — the signature of measuring nothing).
        # If these stubs ever gain distinct paths, this benchmark silently stops testing the
        # dimension while still looking like it does.
        stubs = {n: s for _, n, s in bd.IMPOSTERS}["BodyField"]
        eqs = [st["predicates"][0]["equals"] for st in stubs]
        self.assertEqual(len({e["path"] for e in eqs}), 1,
                         "BodyField stubs must share ONE path, or the path dimension prunes first "
                         "and this scenario measures overhead instead of the body-field index")
        self.assertEqual(len({e["method"] for e in eqs}), 1, "and one method, for the same reason")
        self.assertEqual(len({e["body"]["orderId"] for e in eqs}), len(stubs),
                         "each stub must differ in the discriminating body field")

    def test_body_field_scaling_retargets_its_scenario(self):
        orig_i, orig_d = list(bd.IMPOSTERS), list(bd.DIMENSION_SCENARIOS)
        try:
            for n in (2, 10, 200, 1000):
                bd.IMPOSTERS, bd.DIMENSION_SCENARIOS = list(orig_i), list(orig_d)
                bd.set_body_field_stub_count(n)
                stubs = {nm: s for _, nm, s in bd.IMPOSTERS}["BodyField"]
                ids = {st["predicates"][0]["equals"]["body"]["orderId"] for st in stubs}
                scen = {s[0]: s for s in bd.DIMENSION_SCENARIOS}["body_field_scale"]
                self.assertIn(json.loads(scen[4])["orderId"], ids,
                              f"n={n}: scenario body targets an orderId no stub serves")
        finally:
            bd.IMPOSTERS, bd.DIMENSION_SCENARIOS = orig_i, orig_d

    def test_method_mix_shares_paths_across_verbs(self):
        # If each verb had its own path, the path dimension would prune first and the method
        # dimension would never be the discriminator.
        stubs = {n: s for _, n, s in bd.IMPOSTERS}["MethodMix"]
        by_path = {}
        for st in stubs:
            eq = st["predicates"][0]["equals"]
            by_path.setdefault(eq["path"], set()).add(eq["method"])
        self.assertTrue(all(len(v) > 1 for v in by_path.values()),
                        "each path must be served by multiple verbs")

    def test_non_get_post_methods_are_now_exercised(self):
        methods = {s[2] for s in bd.SCENARIOS + bd.DIMENSION_SCENARIOS}
        self.assertTrue(methods - {"GET", "POST"},
                        "no scenario uses a verb beyond GET/POST, so #729's method dimension "
                        "is still unmeasured")


class StubCountAxis(unittest.TestCase):
    """Issue #779: the dimension replaces an O(N) scan, so N is the axis."""

    def test_scaling_rebuilds_only_the_jsonbody_imposter(self):
        original = list(bd.IMPOSTERS)
        try:
            bd.set_json_body_stub_count(7)
            by_name = {name: stubs for _, name, stubs in bd.IMPOSTERS}
            self.assertEqual(len(by_name["JSONBody"]), 7)
            # every other imposter is untouched, or the A/B varies more than one thing
            for _, name, stubs in original:
                if name != "JSONBody":
                    self.assertEqual(len(by_name[name]), len(stubs), f"{name} changed")
        finally:
            bd.IMPOSTERS = original

    def test_default_count_is_the_pre_779_value(self):
        self.assertEqual(len(bd.json_body_stubs()), bd.DEFAULT_JSON_BODY_STUBS)

    def test_scenario_target_still_exists_after_scaling(self):
        # The bug this guards: scaling the imposter's stubs without retargeting the scenario left
        # json_body_equals addressing /json/equals/25 against a 10-stub imposter. The request fell
        # through to the no-match default and the run aborted on the body assertion — after paying
        # for a full release build in CI.
        original_imposters, original_scenarios = list(bd.IMPOSTERS), list(bd.SCENARIOS)
        try:
            for n in (1, 2, 10, 50, 1000):
                bd.IMPOSTERS, bd.SCENARIOS = list(original_imposters), list(original_scenarios)
                bd.set_json_body_stub_count(n)
                stubs = {name: s for _, name, s in bd.IMPOSTERS}["JSONBody"]
                paths = {st["predicates"][0]["equals"]["path"] for st in stubs}
                scen = {s[0]: s for s in bd.SCENARIOS}["json_body_equals"]
                self.assertIn(scen[3], paths,
                              f"n={n}: scenario targets {scen[3]}, which no stub serves")
                # the body must address the same stub as the path, or the equals predicate misses
                self.assertEqual(json.loads(scen[4])["id"], int(scen[3].rsplit("/", 1)[1]))
        finally:
            bd.IMPOSTERS, bd.SCENARIOS = original_imposters, original_scenarios

    def test_default_scaling_is_byte_identical_to_the_unscaled_scenario(self):
        original_imposters, original_scenarios = list(bd.IMPOSTERS), list(bd.SCENARIOS)
        before = {s[0]: s for s in bd.SCENARIOS}["json_body_equals"]
        try:
            bd.set_json_body_stub_count(bd.DEFAULT_JSON_BODY_STUBS)
            after = {s[0]: s for s in bd.SCENARIOS}["json_body_equals"]
            self.assertEqual(before[3], after[3])
            self.assertEqual(json.loads(before[4]), json.loads(after[4]))
        finally:
            bd.IMPOSTERS, bd.SCENARIOS = original_imposters, original_scenarios


class BinarySize(unittest.TestCase):
    def test_missing_binary_reads_none_not_zero(self):
        # A zero would render as a real measurement of "no bytes"; absent must stay absent.
        self.assertIsNone(bd.binary_size_mb("/nonexistent/rift-http-proxy"))

    def test_reports_megabytes(self):
        import tempfile
        with tempfile.NamedTemporaryFile() as f:
            f.write(b"x" * (2 * 1024 * 1024))
            f.flush()
            self.assertEqual(bd.binary_size_mb(f.name), 2.0)


class CpuTopology(unittest.TestCase):
    """Issue #746 core-count axis: `lscpu -p=CPU,CORE` parsing. Real output carries a comment
    header that must not be read as data."""

    def test_parses_cpu_core_pairs(self):
        pairs = bd.parse_lscpu_topology("# hdr\n0,0\n1,0\n2,1\n3,1\n")
        self.assertEqual(pairs, [(0, 0), (1, 0), (2, 1), (3, 1)])

    def test_ignores_extra_columns(self):
        self.assertEqual(bd.parse_lscpu_topology("0,0,0,0\n1,1,0,0\n"), [(0, 0), (1, 1)])

    def test_rejects_empty(self):
        with self.assertRaises(ValueError):
            bd.parse_lscpu_topology("# only comments\n")


class PlanCpuSplit(unittest.TestCase):
    """Issue #746: the engine/generator split must fall on physical-core boundaries — handing oha
    the SMT sibling of an engine core is the contention this axis exists to eliminate."""

    # 8 physical cores x 2 threads, siblings interleaved (0/8, 1/9, ...) as on the 16-core runner.
    SMT = [(c, c % 8) for c in range(16)]
    # A no-SMT host: 8 cores, 8 CPUs.
    FLAT = [(c, c) for c in range(8)]

    def test_splits_smt_host_on_core_boundary(self):
        engine, gen = bd.plan_cpu_split(self.SMT, 8)
        self.assertEqual(engine, [0, 1, 2, 3, 8, 9, 10, 11])   # 4 physical cores, both siblings
        self.assertEqual(gen, [4, 5, 6, 7, 12, 13, 14, 15])

    def test_no_cpu_is_in_both_sets(self):
        engine, gen = bd.plan_cpu_split(self.SMT, 4)
        self.assertEqual(set(engine) & set(gen), set())
        self.assertEqual(sorted(engine + gen), list(range(16)))

    def test_no_physical_core_is_shared(self):
        engine, gen = bd.plan_cpu_split(self.SMT, 4)
        core_of = dict(self.SMT)
        self.assertEqual({core_of[c] for c in engine} & {core_of[c] for c in gen}, set())

    def test_flat_host_splits_per_cpu(self):
        engine, gen = bd.plan_cpu_split(self.FLAT, 2)
        self.assertEqual(engine, [0, 1])
        self.assertEqual(gen, [2, 3, 4, 5, 6, 7])

    def test_rejects_budget_off_core_boundary(self):
        # 1 CPU would split a hyperthread pair, leaving the sibling to the generator.
        with self.assertRaises(ValueError) as cm:
            bd.plan_cpu_split(self.SMT, 1)
        self.assertIn("core boundary", str(cm.exception))

    def test_rejects_budget_starving_the_generator(self):
        with self.assertRaises(ValueError) as cm:
            bd.plan_cpu_split(self.FLAT, 8)
        self.assertIn("load generator", str(cm.exception))


class TasksetPrefix(unittest.TestCase):
    """Issue #746: pinning is a launcher prefix; absent a core budget it must vanish entirely so
    the historical unpinned runs are reproduced byte-for-byte."""

    def test_prefix_for_cpus(self):
        self.assertEqual(bd.taskset_prefix([0, 1, 8, 9]), ["taskset", "-c", "0,1,8,9"])

    def test_no_cpus_means_no_prefix(self):
        self.assertEqual(bd.taskset_prefix(None), [])
        self.assertEqual(bd.taskset_prefix([]), [])

    def test_oha_cmd_is_unchanged_without_a_prefix(self):
        self.assertEqual(bd.build_oha_cmd("http://x/", "GET", None, {}, "20s", 50)[0], "oha")

    def test_oha_cmd_runs_under_the_prefix(self):
        cmd = bd.build_oha_cmd("http://x/", "GET", None, {}, "20s", 50,
                               prefix=["taskset", "-c", "4,5"])
        self.assertEqual(cmd[:3], ["taskset", "-c", "4,5"])
        self.assertEqual(cmd[3], "oha")
        self.assertEqual(cmd[-1], "http://x/")




class RssSamplerLifecycle(unittest.TestCase):
    """Issue #746 follow-up: RssSampler must not shadow threading.Thread internals.
    Naming its stop flag `_stop` broke every `join()` on Python <=3.12
    (`Thread._stop()` is called internally) — invisible on Python 3.13 dev boxes."""

    def test_start_sample_stop_joins_cleanly(self):
        sampler = bd.RssSampler(os.getpid())
        sampler.start()
        import time as _time
        _time.sleep(1.3)  # at least one sample tick
        sampler.stop()    # join() raised TypeError on 3.12 before the rename
        self.assertFalse(sampler.is_alive())

    def test_no_thread_internal_shadowing(self):
        sampler = bd.RssSampler(os.getpid())
        shadowed = [a for a in ("_stop", "_started", "_tstate_lock", "_handle")
                    if type(sampler).__mro__[0] is bd.RssSampler
                    and a in sampler.__dict__ and hasattr(threading.Thread, a)]
        self.assertEqual(shadowed, [], f"Thread internals shadowed: {shadowed}")


if __name__ == "__main__":
    unittest.main()
