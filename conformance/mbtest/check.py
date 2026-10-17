#!/usr/bin/env python3
"""Hold one engine's mbTest outcome against expected.json (#1547). See README.md.

usage:
  check.py --engine rift|mountebank --out OUT_DIR [--mbtest-dir DIR] [--expected FILE] [--summary FILE]
  check.py --self-test

Reads OUT_DIR/enumeration.json and OUT_DIR/enumeration-docs.json (mocha --dry-run: every test the
suite holds) and OUT_DIR/<engine>/{api,cli,docs}.json (mocha's json reporter), writes
OUT_DIR/<engine>/results.json, and exits non-zero when:

  for Rift
  - a test failed, or never ran, and expected.json does not list it;
  - a test expected.json lists now passes (remove the entry: the list only shrinks), unless the
    entry carries `flaky` (why its outcome varies from run to run);
  for Mountebank (the baseline)
  - a test failed, or never ran, and expected.json does not list it as `harness`, nor mark it
    `mountebankMayFail` (why its Mountebank outcome depends on the host, e.g. its network);
  for both
  - an expected.json or unsupported id names no test in the suite (renamed upstream);
  - an entry is malformed: a `bug` without `issue`, a `documented` whose `doc` anchor does not
    exist, a `harness` without `reason`, or an id both expected and unsupported;
  - a *Test.js file in the checkout is neither run nor unsupported;
  - a phase wrote no results.

A test id is `<file under mbTest>::<mocha full title>`, or `docs:<page>#<scenario>`.
"""
from __future__ import annotations

import argparse
import fnmatch
import json
import os
import re
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
PHASES = ("api", "cli", "docs")
CLASSES = ("bug", "documented", "harness")
DOCS_ADAPTER = "docsScenarios.js"


class CheckError(Exception):
    pass


# --- ids ---------------------------------------------------------------------------------------


def test_id(test: dict) -> str:
    file = test.get("file") or ""
    if file.endswith(DOCS_ADAPTER):
        # docsScenarios.js: one suite per page (no spaces in a page path), one test per scenario.
        page, _, scenario = test["fullTitle"].partition(" ")
        return f"docs:{page}#{scenario}"
    marker = "/mbTest/"
    rel = file[file.rindex(marker) + len(marker):] if marker in file else file
    return f"{rel}::{test['fullTitle']}"


def is_hook(test: dict) -> bool:
    return bool(re.match(r'^"(before|after)( each|all)?" hook', test.get("title", "")))


def load_json(path: Path) -> dict:
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        raise CheckError(f"{path}: missing (did the phase run?)") from None
    except json.JSONDecodeError as e:
        raise CheckError(f"{path}: not JSON ({e})") from None


def enumerate_suite(out: Path) -> list[str]:
    ids: list[str] = []
    for name in ("enumeration.json", "enumeration-docs.json"):
        ids += [test_id(t) for t in load_json(out / name)["tests"]]
    if len(ids) != len(set(ids)):
        dupes = sorted({i for i in ids if ids.count(i) > 1})
        raise CheckError(f"duplicate test ids in the suite: {dupes}")
    return ids


def outcomes(engine_dir: Path, errors: list[str]) -> tuple[dict[str, str], dict[str, str], list[str]]:
    """id -> pass|fail|skipped, id -> first failure line, hook failures."""
    status: dict[str, str] = {}
    message: dict[str, str] = {}
    hooks: list[str] = []
    for phase in PHASES:
        try:
            report = load_json(engine_dir / f"{phase}.json")
        except CheckError as e:
            errors.append(str(e))
            continue
        for t in report.get("passes", []):
            status.setdefault(test_id(t), "pass")
        for t in report.get("pending", []):
            status.setdefault(test_id(t), "skipped")
        for t in report.get("failures", []):
            first = ((t.get("err") or {}).get("message") or "").strip().split("\n")[0][:200]
            if is_hook(t):
                hooks.append(f"{test_id(t)}: {first}")
                continue
            # A test can pass and then be failed by an uncaught error from its hooks: fail wins.
            status[test_id(t)] = "fail"
            message.setdefault(test_id(t), first)
    return status, message, hooks


# --- expected.json -----------------------------------------------------------------------------


def slugify(heading: str) -> str:
    """GitHub-style heading ids, as mkdocs.yml configures pymdownx.slugs (case: lower)."""
    text = re.sub(r"<[^>]+>", "", heading).strip().lower()
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


def anchors(doc: Path) -> set[str]:
    found = set()
    in_fence = False
    for line in doc.read_text().splitlines():
        if line.startswith("```"):
            in_fence = not in_fence
        elif not in_fence and (m := re.match(r"^#{1,6}\s+(.*?)\s*#*\s*$", line)):
            found.add(slugify(m.group(1)))
    return found


def validate_expected(expected: dict, repo_root: Path) -> list[str]:
    errors = []
    seen = set()
    for entry in expected.get("expected", []):
        eid, cls = entry.get("id"), entry.get("class")
        where = f"expected.json {eid!r}"
        if not eid:
            errors.append(f"expected.json: an entry has no id: {entry}")
            continue
        if eid in seen:
            errors.append(f"{where}: listed twice")
        seen.add(eid)
        if cls not in CLASSES:
            errors.append(f"{where}: class must be one of {CLASSES}, not {cls!r}")
        elif cls == "bug" and not re.fullmatch(r"#\d+|TBD-[a-z0-9-]+", str(entry.get("issue", ""))):
            errors.append(f"{where}: a bug needs \"issue\": \"#<number>\"")
        elif cls == "documented":
            doc = str(entry.get("doc", ""))
            path, _, anchor = doc.partition("#")
            if not anchor:
                errors.append(f"{where}: a documented entry needs \"doc\": \"<file>#<anchor>\"")
            elif not (repo_root / path).is_file():
                errors.append(f"{where}: {path} does not exist")
            elif anchor not in anchors(repo_root / path):
                errors.append(f"{where}: {path} has no heading #{anchor}")
        elif cls == "harness" and not entry.get("reason"):
            errors.append(f"{where}: a harness entry needs a reason")
    for entry in expected.get("unsupported", []):
        if not entry.get("glob") or not entry.get("reason"):
            errors.append(f"expected.json unsupported entry needs glob and reason: {entry}")
    for entry in expected.get("expected", []) + expected.get("unsupported", []):
        # Any cited doc anchor must resolve, whatever the entry's class.
        doc = str(entry.get("doc", ""))
        path, _, anchor = doc.partition("#")
        if doc and entry.get("class") != "documented" and (
                not (repo_root / path).is_file() or (anchor and anchor not in anchors(repo_root / path))):
            errors.append(f"expected.json {entry.get('id') or entry.get('glob')!r}: {doc} does not resolve")
    return errors


def unsupported_match(glob: str, tid: str) -> bool:
    if "::" in glob or glob.startswith("docs:"):
        return fnmatch.fnmatchcase(tid, glob)
    # A glob with no `::` names test files.
    return not tid.startswith("docs:") and fnmatch.fnmatchcase(tid.split("::", 1)[0], glob)


# --- the check ---------------------------------------------------------------------------------


def check(engine: str, out: Path, expected_path: Path, mbtest_dir: Path | None,
          repo_root: Path = REPO_ROOT) -> tuple[list[str], dict]:
    errors: list[str] = []
    expected = load_json(expected_path)
    errors += validate_expected(expected, repo_root)
    suite = enumerate_suite(out)
    suite_set = set(suite)
    globs = expected.get("unsupported", [])
    entries = {e["id"]: e for e in expected.get("expected", []) if e.get("id")}

    unsupported = {t for t in suite if any(unsupported_match(g["glob"], t) for g in globs)}
    for g in globs:
        if not any(unsupported_match(g["glob"], t) for t in suite) and not (
                mbtest_dir and any(fnmatch.fnmatchcase(f, g["glob"]) for f in test_files(mbtest_dir))):
            errors.append(f"unsupported glob {g['glob']!r} matches nothing in the suite")
    for eid in entries:
        if eid not in suite_set:
            errors.append(f"expected.json {eid!r}: no such test in mbTest (renamed upstream?)")
        elif eid in unsupported:
            errors.append(f"expected.json {eid!r}: also matched by an unsupported glob")

    if mbtest_dir:
        run_files = {t.split("::", 1)[0] for t in suite if not t.startswith("docs:")}
        for f in test_files(mbtest_dir):
            if f not in run_files and not any(unsupported_match(g["glob"], f + "::") for g in globs):
                errors.append(f"{f}: a test file that is neither run nor listed as unsupported")

    status, message, hooks = outcomes(out / engine, errors)
    results = {}
    counts = {"pass": 0, "fail": 0, "notrun": 0, "skipped": 0, "unsupported": 0}
    unexpected, stale = [], []
    for tid in suite:
        outcome = "unsupported" if tid in unsupported else status.get(tid, "notrun")
        counts[outcome] += 1
        results[tid] = {"outcome": outcome, **({"message": message[tid]} if tid in message else {})}
        entry = entries.get(tid)
        failed = outcome in ("fail", "notrun")
        if engine == "mountebank":
            if failed and (entry is None or (entry.get("class") != "harness"
                                             and not entry.get("mountebankMayFail"))):
                unexpected.append(f"{tid} [{outcome}] {message.get(tid, '')}".rstrip()
                                  + "  -- fails on Mountebank, so it can only be a `harness` entry")
        elif failed and entry is None:
            unexpected.append(f"{tid} [{outcome}] {message.get(tid, '')}".rstrip())
        elif outcome == "pass" and entry is not None and not entry.get("flaky"):
            stale.append(f"{tid} now passes on Rift: remove it from expected.json ({entry.get('class')}"
                         f" {entry.get('issue') or entry.get('doc') or ''})".rstrip())

    # A harness entry is for a test that cannot pass anywhere: it must fail on Mountebank too, unless
    # its entry says (onMountebank: "passes") why it cannot run against a foreign binary.
    baseline = out / "mountebank" / "results.json"
    if engine == "rift" and baseline.is_file():
        mb = load_json(baseline)["tests"]
        for eid, entry in entries.items():
            if entry.get("class") == "harness" and mb.get(eid, {}).get("outcome") == "pass" \
                    and entry.get("onMountebank") != "passes" and not entry.get("flaky"):
                errors.append(f"expected.json {eid!r}: harness, but it passes on Mountebank; reclassify"
                              " it, or say why no foreign binary can pass it (onMountebank: passes)")

    errors += [f"unexpected failure: {u}" for u in unexpected]
    errors += [f"stale entry: {s}" for s in stale]
    classes = {c: sum(1 for e in entries.values() if e.get("class") == c) for c in CLASSES}
    report = {"engine": engine, "mbtestRef": expected.get("mbtestRef"), "counts": counts,
              "expectedByClass": classes, "hookFailures": hooks, "tests": results}
    return errors, report


def test_files(mbtest_dir: Path) -> list[str]:
    files = []
    for root, dirs, names in os.walk(mbtest_dir):
        dirs[:] = [d for d in dirs if d != "node_modules"]
        files += [os.path.relpath(os.path.join(root, n), mbtest_dir) for n in names if n.endswith("Test.js")]
    return sorted(files)


def summary_markdown(report: dict, errors: list[str]) -> str:
    c = report["counts"]
    per_phase: dict[str, dict[str, int]] = {}
    for tid, r in report["tests"].items():
        phase = "docs" if tid.startswith("docs:") else tid.split("/", 1)[0]
        per_phase.setdefault(phase, {}).setdefault(r["outcome"], 0)
        per_phase[phase][r["outcome"]] += 1
    lines = [f"### mbTest {report['mbtestRef']} against {report['engine']}", "",
             "| phase | pass | fail | not run | skipped | unsupported |", "|:--|--:|--:|--:|--:|--:|"]
    for phase in sorted(per_phase):
        p = per_phase[phase]
        lines.append(f"| {phase} | {p.get('pass', 0)} | {p.get('fail', 0)} | {p.get('notrun', 0)} | "
                     f"{p.get('skipped', 0)} | {p.get('unsupported', 0)} |")
    lines.append(f"| **all** | {c['pass']} | {c['fail']} | {c['notrun']} | {c['skipped']} | {c['unsupported']} |")
    k = report["expectedByClass"]
    lines += ["", f"expected.json: {k['bug']} bug, {k['documented']} documented, {k['harness']} harness.", ""]
    lines.append("**OK**: every failure is expected, and no expected entry passes." if not errors
                 else f"**FAILED** ({len(errors)}):\n" + "\n".join(f"- {e}" for e in errors))
    return "\n".join(lines) + "\n"


# --- self-test ---------------------------------------------------------------------------------


def self_test() -> int:
    def mocha(tests: list[tuple[str, str, str]]) -> dict:
        def t(file: str, title: str) -> dict:
            return {"title": title.split(" ")[-1], "fullTitle": title, "file": f"/x/mbTest/{file}",
                    "err": {"message": "boom"}}
        return {"passes": [t(f, n) for f, n, o in tests if o == "pass"],
                "failures": [t(f, n) for f, n, o in tests if o == "fail"],
                "pending": [t(f, n) for f, n, o in tests if o == "skip"],
                "tests": [t(f, n) for f, n, _ in tests]}

    suite = [("api/aTest.js", "a passes", ""), ("api/aTest.js", "a fails", ""),
             ("cli/bTest.js", "b fails", ""), ("api/tcp/tTest.js", "t any", "")]
    docs_suite = [("/m/conformance/mbtest/docsScenarios.js", "/docs/api/mocks example", "")]
    good = {"mbtestRef": "v0.0.0",
            "unsupported": [{"glob": "api/tcp/**", "reason": "no tcp"}],
            "expected": [{"id": "api/aTest.js::a fails", "class": "bug", "issue": "#1"},
                         {"id": "cli/bTest.js::b fails", "class": "documented", "doc": "doc.md#a-heading"},
                         {"id": "docs:/docs/api/mocks#example", "class": "harness", "reason": "r"}]}
    outcome = {"api": [("api/aTest.js", "a passes", "pass"), ("api/aTest.js", "a fails", "fail"),
                       ("api/tcp/tTest.js", "t any", "fail")],
               "cli": [("cli/bTest.js", "b fails", "fail")],
               "docs": [("/m/conformance/mbtest/docsScenarios.js", "/docs/api/mocks example", "fail")]}

    def run(expected: dict, outcome: dict, engine: str = "rift") -> list[str]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "doc.md").write_text("# Title\n\n## A heading\n\n```\n## Not a heading\n```\n")
            (root / engine).mkdir()
            (root / "enumeration.json").write_text(json.dumps(mocha(suite)))
            (root / "enumeration-docs.json").write_text(json.dumps(mocha(docs_suite)))
            for phase, tests in outcome.items():
                (root / engine / f"{phase}.json").write_text(json.dumps(mocha(tests)))
            (root / "expected.json").write_text(json.dumps(expected))
            errors, _ = check(engine, root, root / "expected.json", None, repo_root=root)
            return errors

    def expect(name: str, errors: list[str], needle: str | None) -> bool:
        ok = (not errors) if needle is None else any(needle in e for e in errors)
        print(f"  {'ok  ' if ok else 'FAIL'} {name}" + ("" if ok else f": {errors}"))
        return ok

    def with_outcome(phase: str, change: dict) -> dict:
        o = json.loads(json.dumps(outcome))
        o[phase] = [(f, n, change.get(n, r)) for f, n, r in o[phase]]
        return o

    def with_expected(**kw) -> dict:
        e = json.loads(json.dumps(good))
        e.update(kw)
        return e

    entries = good["expected"]
    cases = [
        ("a consistent run passes", run(good, outcome), None),
        ("an unlisted failure fails", run(good, with_outcome("api", {"a passes": "fail"})), "unexpected failure"),
        ("a test that never ran fails", run(good, {**outcome, "api": outcome["api"][1:]}), "[notrun]"),
        ("a listed test that passes is stale", run(good, with_outcome("api", {"a fails": "pass"})), "stale entry"),
        ("a flaky entry that passes is not stale", run(with_expected(expected=[{**entries[0], "flaky": "varies"}]
                                                                    + entries[1:]),
                                                     with_outcome("api", {"a fails": "pass"})), None),
        ("an unknown id fails", run(with_expected(expected=entries + [
            {"id": "api/aTest.js::renamed", "class": "bug", "issue": "#2"}]), outcome), "no such test"),
        ("a missing doc anchor fails", run(with_expected(expected=[entries[0], {**entries[1], "doc": "doc.md#gone"},
                                                                     entries[2]]), outcome), "has no heading"),
        ("a heading inside a code fence is no anchor", run(with_expected(expected=[
            entries[0], {**entries[1], "doc": "doc.md#not-a-heading"}, entries[2]]), outcome), "has no heading"),
        ("a bug without an issue fails", run(with_expected(expected=[{**entries[0], "issue": ""}] + entries[1:]),
                                             outcome), "needs \"issue\""),
        ("a harness without a reason fails", run(with_expected(expected=entries[:2] + [
            {**entries[2], "reason": ""}]), outcome), "needs a reason"),
        ("a missing phase fails", run(good, {k: v for k, v in outcome.items() if k != "docs"}), "missing"),
        ("Mountebank: a non-harness failure fails", run(good, outcome, "mountebank"), "only be a `harness`"),
        ("Mountebank: a mountebankMayFail entry may fail", run(with_expected(expected=[
            e if e.get("class") == "harness" else {**e, "mountebankMayFail": "host network"} for e in entries]),
            outcome, "mountebank"), None),
        ("Mountebank: harness failures only pass", run(good, with_outcome("cli", {"b fails": "pass"}) | {
            "api": [("api/aTest.js", "a passes", "pass"), ("api/aTest.js", "a fails", "pass")]}, "mountebank"), None),
    ]
    failed = sum(not expect(*c) for c in cases)
    print(f"self-test: {len(cases) - failed}/{len(cases)} ok")
    return 1 if failed else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--engine", choices=("rift", "mountebank"))
    ap.add_argument("--out", type=Path)
    ap.add_argument("--expected", type=Path, default=HERE / "expected.json")
    ap.add_argument("--mbtest-dir", type=Path)
    ap.add_argument("--summary", type=Path, help="append a markdown summary (e.g. $GITHUB_STEP_SUMMARY)")
    args = ap.parse_args()
    if args.self_test:
        return self_test()
    if not args.engine or not args.out:
        ap.error("--engine and --out are required")
    try:
        errors, report = check(args.engine, args.out, args.expected, args.mbtest_dir)
    except CheckError as e:
        print(f"check.py: {e}", file=sys.stderr)
        return 2
    (args.out / args.engine / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    text = summary_markdown(report, errors)
    print(text)
    if args.summary:
        with open(args.summary, "a") as f:
            f.write(text)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
