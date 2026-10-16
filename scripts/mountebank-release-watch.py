#!/usr/bin/env python3
"""Open an issue when Mountebank ships a stable release newer than the oracle pin (issue #1454).

Run by .github/workflows/mountebank-release-watch.yml. It compares npm's `latest` dist-tag of
@mbtest/mountebank with `MB_VERSION` in .github/workflows/ci.yml and, when they differ, opens one
issue titled `Mountebank <version> released` unless an open issue already has that exact title.
It never edits the pin: moving the oracle is a reviewed change, since a new Mountebank can change
what Rift must do.

    scripts/mountebank-release-watch.py --dry-run            # print the decision, touch nothing
    scripts/mountebank-release-watch.py --dry-run --pinned 2.9.4
    scripts/mountebank-release-watch.py --repo owner/name    # open the issue if one is due
    scripts/mountebank-release-watch.py --self-test

Any failure to read either version exits non-zero: an empty or unparsable version is an error,
never a difference, so the watch cannot open an issue for "".
"""
import argparse, json, os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CI_YML = os.path.join(ROOT, ".github", "workflows", "ci.yml")
PACKAGE = "@mbtest/mountebank"
LABELS = ["compatibility", "needs-triage"]
STABLE = re.compile(r"^\d+\.\d+\.\d+$")
PIN = re.compile(r'^\s*MB_VERSION:\s*"([^"]*)"\s*(?:#.*)?$', re.M)


class WatchError(Exception):
    pass


def stable(version, source):
    """The version, if it is an exact stable x.y.z; otherwise a WatchError naming the source."""
    if not STABLE.match(version):
        raise WatchError(f"{source} gave {version!r}, which is not a stable x.y.z version")
    return version


def parse_pin(ci_text):
    """The one `MB_VERSION: "x.y.z"` in ci.yml. Zero or several pins is an error."""
    found = PIN.findall(ci_text)
    if len(found) != 1:
        raise WatchError(f"expected exactly one MB_VERSION pin in ci.yml, found {len(found)}")
    return stable(found[0], "MB_VERSION in ci.yml")


def run(cmd):
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, check=False)
    except OSError as e:
        raise WatchError(f"{cmd[0]}: {e}") from e
    if out.returncode != 0:
        raise WatchError(f"{' '.join(cmd)} exited {out.returncode}: {out.stderr.strip()}")
    return out.stdout.strip()


def npm_latest():
    return stable(run(["npm", "view", PACKAGE, "dist-tags.latest"]), f"npm's {PACKAGE} latest")


def title_for(version):
    return f"Mountebank {version} released"


def body_for(version, pinned):
    return f"""Mountebank **{version}** is npm's `latest` release of `{PACKAGE}`; Rift's oracle pins **{pinned}** (`MB_VERSION` in `.github/workflows/ci.yml`).

- npm: https://www.npmjs.com/package/{PACKAGE}/v/{version}
- releases: https://github.com/mountebank-testing/mountebank/releases

Before moving the pin:

- [ ] Run the differential corpus against it: `npm install --prefix ~/bench-mb {PACKAGE}@{version}` then `cargo test -p rift-differential -- --nocapture`; every new difference is either a Rift bug or a documented allow-list entry, and every entry the harness reports stale is deleted.
- [ ] Probe the dependency jumps between {pinned} and {version} (`npm diff --diff={PACKAGE}@{pinned} --diff={PACKAGE}@{version} -- package.json`) for behaviour Rift mirrors.
- [ ] Bump `MB_VERSION` in `ci.yml`, the `mb_version` default in `benchmark-publish.yml`, and the install lines that name {pinned}.
- [ ] Update every "compatible with **Mountebank {pinned}**" statement (README.md, docs/index.md, docs/mountebank/index.md, docs/getting-started/migration.md) and `docs/mountebank/differences.md`; `scripts/verify-mountebank-version-docs.sh` fails CI until they match `MB_VERSION`.

<sub>Opened by `.github/workflows/mountebank-release-watch.yml` (#1454). It never edits the pin.</sub>
"""


def open_issue_titled(title, repo):
    """True when an open issue has exactly this title."""
    cmd = ["gh", "issue", "list", "--state", "open", "--search", f'"{title}" in:title',
           "--limit", "100", "--json", "title"]
    if repo:
        cmd += ["--repo", repo]
    try:
        issues = json.loads(run(cmd))
    except json.JSONDecodeError as e:
        raise WatchError(f"gh issue list returned unparsable JSON: {e}") from e
    return any(i.get("title") == title for i in issues)


def decide(pinned, latest):
    """Act only on a newer release: a dist-tag rolled back below the pin is not news."""
    as_tuple = lambda v: tuple(int(part) for part in v.split("."))
    return "differs" if as_tuple(latest) > as_tuple(pinned) else "up to date"


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--dry-run", action="store_true", help="print the decision; create nothing")
    ap.add_argument("--pinned", help="use this pin instead of ci.yml's MB_VERSION (testing)")
    ap.add_argument("--latest", help="use this version instead of npm's latest (testing)")
    ap.add_argument("--repo", help="owner/name for gh (default: the current checkout's)")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)
    if args.self_test:
        return self_test()

    try:
        if args.pinned is not None:
            pinned = stable(args.pinned, "--pinned")
        else:
            with open(CI_YML, encoding="utf-8") as f:
                pinned = parse_pin(f.read())
        latest = stable(args.latest, "--latest") if args.latest is not None else npm_latest()
        print(f"pinned {pinned}, npm latest {latest}")
        if decide(pinned, latest) == "up to date":
            print("decision: up to date")
            return 0
        title = title_for(latest)
        if open_issue_titled(title, args.repo):
            print(f"decision: already reported (an open issue is titled {title!r})")
            return 0
        if args.dry_run:
            print(f"decision: would open {title!r} with labels {', '.join(LABELS)}")
            return 0
        cmd = ["gh", "issue", "create", "--title", title, "--body", body_for(latest, pinned)]
        for label in LABELS:
            cmd += ["--label", label]
        if args.repo:
            cmd += ["--repo", args.repo]
        print(f"decision: opened {run(cmd)}")
        return 0
    except (WatchError, OSError) as e:
        print(f"mountebank-release-watch: {e}", file=sys.stderr)
        return 1


def self_test():
    ok = parse_pin('env:\n  MB_VERSION: "2.9.5"\n') == "2.9.5"
    ok &= parse_pin('      MB_VERSION: "2.9.5"  # pinned\n') == "2.9.5"
    for bad in ['MB_VERSION: ""\n', 'MB_VERSION: "2.9.6-beta.1"\n', "no pin here\n",
                'MB_VERSION: "2.9.5"\nMB_VERSION: "2.9.4"\n']:
        try:
            parse_pin(bad)
            print(f"self-test: accepted {bad!r}")
            ok = False
        except WatchError:
            pass
    for bad in ["", "latest", "2.9", "2.9.6-beta.10602"]:
        try:
            stable(bad, "test")
            print(f"self-test: accepted version {bad!r}")
            ok = False
        except WatchError:
            pass
    ok &= decide("2.9.5", "2.9.5") == "up to date" and decide("2.9.4", "2.9.5") == "differs"
    ok &= decide("2.9.5", "2.9.4") == "up to date" and decide("2.9.10", "2.9.9") == "up to date"
    ok &= decide("2.9.9", "2.9.10") == "differs"
    ok &= title_for("2.9.6") == "Mountebank 2.9.6 released"
    print("self-test: ok" if ok else "self-test: FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
