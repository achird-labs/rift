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


def point(arm, shape, n, create, get, rss, warnings=0):
    """One measured round for a given (arm, shape, N) — the four points a README row needs."""
    return {"arm": arm, "shape": shape, "n": n, "round": 1, "create_ms": create,
            "get_med_ms": get, "rss_delta_mb": rss, "body_mb": 0.4, "warnings": warnings}


def full_run(*extra_arms):
    """Every (shape, N) point for `mb` plus each named Rift arm, with distinguishable numbers."""
    samples = []
    for arm in ("mb",) + extra_arms:
        mb = arm == "mb"
        for shape, _ in ba.SHAPES:
            for n in ba.SIZES:
                # Mountebank is deliberately the slower, fatter arm so a swapped arrow is visible.
                base = 40.0 if mb else 5.0
                bump = 0.0 if n == 100 else 30.0
                samples.append(point(arm, shape, n,
                                     create=base + bump,
                                     get=2.0 if mb else 1.0,
                                     rss=50.0 if mb else 9.0,
                                     warnings=0 if mb else 99))
    return samples


class ReadmeRows(unittest.TestCase):
    """Issue #1208: the README's admin table was transcribed by hand, and a header rewrite
    re-dated figures it had not re-measured. These pin the generated rows so the table is pasted."""

    def section(self, text):
        lines = text.splitlines()
        if "## README rows" not in lines:
            return None
        return lines[lines.index("## README rows"):]

    def test_readme_rows_are_emitted_for_an_mb_plus_one_rift_run(self):
        text = ba.render_report(ba.aggregate(full_run("rift")), ["mb", "rift"],
                                {"mb": "2.9.1", "rift": "rift 0.1.0"}, 9, "2026-09-22")
        section = self.section(text)
        self.assertIsNotNone(section, "an mb + one-Rift run must emit the README rows")
        rows = [ln for ln in section if ln.startswith("| identical |")
                or ln.startswith("| distinct |")]
        # Four rows, in the README's order, with the README's shape label (not `identical/overlap`),
        # one decimal, Mountebank on the left of each arrow and Rift's warnings last.
        self.assertEqual(rows, [
            "| identical | 100 | 40.0 → 5.0 | 2.0 → 1.0 | 50.0 → 9.0 | 99 |",
            "| identical | 1000 | 70.0 → 35.0 | 2.0 → 1.0 | 50.0 → 9.0 | 99 |",
            "| distinct | 100 | 40.0 → 5.0 | 2.0 → 1.0 | 50.0 → 9.0 | 99 |",
            "| distinct | 1000 | 70.0 → 35.0 | 2.0 → 1.0 | 50.0 → 9.0 | 99 |",
        ])
        caption = "\n".join(section)
        self.assertIn("2026-09-22", caption)
        self.assertIn("--rep 9", caption)
        self.assertIn("2.9.1", caption)

    def test_readme_rows_are_omitted_for_a_rift_only_ab(self):
        samples = [s for s in full_run("old", "new") if s["arm"] != "mb"]
        text = ba.render_report(ba.aggregate(samples), ["old", "new"],
                                {"old": "a", "new": "b"}, 9, "2026-09-22")
        self.assertIsNone(self.section(text),
                          "a Rift-only A/B has no Mountebank column, so it must emit no rows")

    def test_readme_rows_are_omitted_when_two_rift_arms_are_ambiguous(self):
        text = ba.render_report(ba.aggregate(full_run("old", "new")), ["mb", "old", "new"],
                                {"mb": "2.9.1", "old": "a", "new": "b"}, 9, "2026-09-22")
        self.assertIsNone(self.section(text),
                          "with two Rift arms there is no single build the row speaks for")

    def test_an_unmeasured_point_says_so_instead_of_publishing_a_number(self):
        """A short table pasted into a four-row slot is the same silent-wrongness this whole
        section exists to prevent, so the row stays and says it has no measurement."""
        samples = [s for s in full_run("rift")
                   if not (s["arm"] == "rift" and s["shape"] == "distinct" and s["n"] == 1000)]
        text = ba.render_report(ba.aggregate(samples), ["mb", "rift"],
                                {"mb": "2.9.1", "rift": "rift 0.1.0"}, 9, "2026-09-22")
        rows = [ln for ln in self.section(text) if ln.startswith("| identical |")
                or ln.startswith("| distinct |")]
        self.assertEqual(len(rows), 4, "every point keeps its row so the table cannot go short")
        [missing] = [ln for ln in rows if ln.startswith("| distinct | 1000 |")]
        self.assertIn("*not measured*", missing)
        self.assertNotIn("→", missing, "an unmeasured point must not render an arrow of numbers")

    def test_a_non_finite_measurement_is_refused_not_rendered(self):
        """`rss_mb` returns NaN when `ps` will not parse, and `_median` picks by sort index, so a
        single bad read usually survives as an ordinary-looking median. This section prints
        medians only, so it is the one place that cannot rely on a spread column to expose it."""
        samples = full_run("rift")
        for s in samples:
            if s["arm"] == "rift" and s["shape"] == "distinct" and s["n"] == 1000:
                s["rss_delta_mb"] = float("nan")
        text = ba.render_report(ba.aggregate(samples), ["mb", "rift"],
                                {"mb": "2.9.1", "rift": "rift 0.1.0"}, 9, "2026-09-22")
        [row] = [ln for ln in self.section(text) if ln.startswith("| distinct | 1000 |")]
        self.assertIn("measurement error", row)
        self.assertNotIn("nan", row.lower(), "a NaN must not reach the pasted table at all")
        # The three sound points are unaffected — one bad read does not void the run.
        good = [ln for ln in self.section(text) if ln.startswith("| identical |")]
        self.assertTrue(all("→" in ln for ln in good), "sound rows must still publish numbers")

    def test_readme_rows_are_omitted_when_mountebank_ran_alone(self):
        """The guard has two arms; a run with no Rift build at all must take the second one
        rather than indexing an empty list."""
        samples = [s for s in full_run("rift") if s["arm"] == "mb"]
        text = ba.render_report(ba.aggregate(samples), ["mb"], {"mb": "2.9.1"}, 9, "2026-09-22")
        self.assertIsNone(self.section(text))


if __name__ == "__main__":
    unittest.main()
