#!/usr/bin/env python3
"""Gate a PR on matcher benchmark regressions (issue #298).

Reads criterion's own saved baselines (`--save-baseline base` / `pr`) from `target/criterion` and
compares the mean estimate of each benchmark. Posts a markdown table to the GitHub job summary and
exits non-zero if any benchmark regressed beyond REGRESSION_THRESHOLD (a relative multiple). Reading
criterion's `estimates.json` directly avoids brittle text parsing; an unpaired or unparseable
benchmark is skipped rather than failing the run.
"""

import glob
import json
import os
import sys

CRITERION_DIR = "target/criterion"
THRESHOLD = float(os.environ.get("REGRESSION_THRESHOLD", "1.25"))


def mean_ns(estimates_path):
    try:
        with open(estimates_path) as fh:
            return float(json.load(fh)["mean"]["point_estimate"])
    except (OSError, ValueError, KeyError, TypeError):
        return None


def main():
    rows = []  # (name, base_ns, pr_ns, ratio)
    for base_path in glob.glob(f"{CRITERION_DIR}/**/base/estimates.json", recursive=True):
        pr_path = base_path.replace("/base/estimates.json", "/pr/estimates.json")
        base = mean_ns(base_path)
        pr = mean_ns(pr_path)
        if base is None or pr is None or base <= 0:
            continue
        name = base_path[len(CRITERION_DIR) + 1 : -len("/base/estimates.json")]
        rows.append((name, base, pr, pr / base))

    if not rows:
        print("No paired benchmarks found to compare; skipping the perf gate.")
        return 0

    rows.sort(key=lambda r: r[3], reverse=True)

    def fmt_ns(v):
        return f"{v/1000:.2f} µs" if v >= 1000 else f"{v:.1f} ns"

    lines = [
        "## Matcher benchmark regression gate",
        "",
        f"Threshold: fail if any benchmark is more than **{(THRESHOLD-1)*100:.0f}%** slower than base.",
        "",
        "| Benchmark | base | PR | change |",
        "|---|--:|--:|--:|",
    ]
    regressed = []
    for name, base, pr, ratio in rows:
        pct = (ratio - 1) * 100
        flag = " ⚠️" if ratio > THRESHOLD else (" 🟢" if ratio < 0.9 else "")
        lines.append(f"| `{name}` | {fmt_ns(base)} | {fmt_ns(pr)} | {pct:+.1f}%{flag} |")
        if ratio > THRESHOLD:
            regressed.append((name, pct))

    summary = "\n".join(lines) + "\n"
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a") as fh:
            fh.write(summary)
    print(summary)

    if regressed:
        worst = ", ".join(f"{n} (+{p:.1f}%)" for n, p in regressed)
        print(f"::error::Benchmark regression beyond {(THRESHOLD-1)*100:.0f}% threshold: {worst}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
