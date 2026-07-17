#!/usr/bin/env python3
"""Gate tests for the pure logic added to bench_direct.py (issue #702).

The live benchmark path (oha + rift under load) is not CI-runnable, so these tests
pin the *logic* the harness relies on: oha command construction (closed vs open-loop),
metric extraction (incl. p999), the connection-sweep parsing, the extended CSV schema
and its round-trip, the recording imposter config, and the journal-depth assertion.

Run: python3 -m unittest test_bench_direct   (from tests/benchmark/scripts)
"""
import io
import os
import sys
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


if __name__ == "__main__":
    unittest.main()
