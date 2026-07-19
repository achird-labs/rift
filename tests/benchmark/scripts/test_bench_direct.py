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
