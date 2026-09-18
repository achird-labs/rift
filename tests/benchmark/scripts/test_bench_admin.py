#!/usr/bin/env python3
"""Gate tests for the pure logic of bench_admin.py (issue #1157).

The live path (fresh engines, admin POSTs) is not CI-runnable, so these pin what the numbers rest
on: arm parsing, the interleaved schedule, the warm-up discard, the aggregation, and that no two
runs share an artefact path.

Run: python3 -m unittest test_bench_admin   (from tests/benchmark/scripts)
"""
import os
import unittest

import bench_admin as ba


class ParseRiftBins(unittest.TestCase):
    def test_bare_path_is_labelled_rift(self):
        self.assertEqual(ba.parse_rift_bins(["/x/rift"]), [("rift", "/x/rift")])

    def test_labelled_arms(self):
        self.assertEqual(ba.parse_rift_bins(["old=/a", "new=/b"]), [("old", "/a"), ("new", "/b")])

    def test_default_is_the_release_binary(self):
        [(label, path)] = ba.parse_rift_bins(None)
        self.assertEqual(label, "rift")
        self.assertTrue(path.endswith(os.path.join("target", "release", "rift-http-proxy")))

    def test_duplicate_labels_are_refused(self):
        with self.assertRaises(ValueError):
            ba.parse_rift_bins(["/a", "/b"])  # both default to `rift`

    def test_mb_label_is_refused(self):
        with self.assertRaises(ValueError):
            ba.parse_rift_bins(["mb=/a"])

    def test_empty_parts_are_refused(self):
        for bad in ("=/a", "old="):
            with self.assertRaises(ValueError):
                ba.parse_rift_bins([bad])


class ParseEngines(unittest.TestCase):
    def test_rift_only(self):
        self.assertEqual(ba.parse_engines("rift"), ["rift"])

    def test_both(self):
        self.assertEqual(ba.parse_engines("rift, mb"), ["rift", "mb"])

    def test_unknown_or_empty_is_refused(self):
        for bad in ("", "wiremock", "rift,x"):
            with self.assertRaises(ValueError):
                ba.parse_engines(bad)


class Schedule(unittest.TestCase):
    def test_every_round_measures_every_point_on_every_arm(self):
        order = ba.schedule(4, ["p", "q"], ["a", "b", "c"])
        self.assertEqual(len(order), 4 * 2 * 3)
        for rnd in range(4):
            got = sorted((p, a) for r, p, a in order if r == rnd)
            self.assertEqual(got, sorted((p, a) for p in "pq" for a in "abc"))

    def test_arms_of_a_point_run_back_to_back(self):
        order = ba.schedule(2, ["p", "q"], ["a", "b"])
        self.assertEqual([p for _, p, _ in order], ["p", "p", "q", "q", "p", "p", "q", "q"])

    def test_the_first_arm_rotates_per_round(self):
        order = ba.schedule(3, ["p"], ["a", "b", "c"])
        firsts = [order[i][2] for i in (0, 3, 6)]
        self.assertEqual(firsts, ["a", "b", "c"])


def sample(arm, create, rss=10.0, shape="distinct", n=100):
    return {"arm": arm, "shape": shape, "n": n, "round": 1, "create_ms": create,
            "get_med_ms": 1.0, "rss_delta_mb": rss, "body_mb": 0.4, "warnings": 0}


class Aggregate(unittest.TestCase):
    def test_median_min_max_and_spread_per_arm(self):
        stats = ba.aggregate([sample("a", c) for c in (3.0, 8.0, 5.0)] + [sample("b", 7.0)])
        a = stats[("a", "distinct", 100)]
        self.assertEqual(a["reps"], 3)
        self.assertEqual(a["create_med_ms"], 5.0)
        self.assertEqual((a["create_min_ms"], a["create_max_ms"]), (3.0, 8.0))
        self.assertAlmostEqual(a["create_spread_pct"], 5.0 / (16.0 / 3) * 100)
        self.assertEqual(stats[("b", "distinct", 100)]["reps"], 1)

    def test_even_count_median_averages_the_middle(self):
        stats = ba.aggregate([sample("a", c) for c in (1.0, 2.0, 4.0, 9.0)])
        self.assertEqual(stats[("a", "distinct", 100)]["create_med_ms"], 3.0)

    def test_points_do_not_mix(self):
        stats = ba.aggregate([sample("a", 1.0, n=100), sample("a", 9.0, n=1000)])
        self.assertEqual(stats[("a", "distinct", 100)]["create_med_ms"], 1.0)
        self.assertEqual(stats[("a", "distinct", 1000)]["create_med_ms"], 9.0)


class Artefacts(unittest.TestCase):
    def test_two_tags_share_no_path(self):
        a, b = ba.output_paths("t1"), ba.output_paths("t2")
        self.assertFalse(set(a.values()) & set(b.values()))

    def test_every_measurement_has_its_own_log(self):
        names = {ba.log_name(arm, shape, n, rnd)
                 for rnd, (shape, n), arm in ba.schedule(
                     3, [(s, n) for s, _ in ba.SHAPES for n in ba.SIZES], ["rift", "mb"])}
        self.assertEqual(len(names), 3 * len(ba.SHAPES) * len(ba.SIZES) * 2)
        self.assertTrue(all("/" not in name for name in names))


class Report(unittest.TestCase):
    def test_report_carries_reps_median_and_spread_for_each_arm(self):
        stats = ba.aggregate([sample("old", c) for c in (3.0, 5.0)] + [sample("new", 4.0)])
        text = ba.render_report(stats, ["old", "new"], {"old": "v1", "new": "v2"}, 2, "today")
        rows = [line for line in text.splitlines() if line.startswith("| 100 |")]
        self.assertEqual(len(rows), 2)
        self.assertIn("| old | 2 | 4.00 | 3.00–5.00 | 50% |", rows[0])
        self.assertIn("| new | 1 | 4.00 |", rows[1])
        self.assertIn("**old:** v1", text)


if __name__ == "__main__":
    unittest.main()
