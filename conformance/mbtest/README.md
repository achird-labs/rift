# mbTest: Mountebank's own suite, run against Rift

[`differential/`](../differential/) checks Rift against cases this repository wrote. This plane runs
the tests **Mountebank's authors** wrote: `mbTest`, the black-box functional suite in Mountebank's
repository, at the tag in [`MBTEST_REF`](MBTEST_REF), including its docs tester, which executes every
example on Mountebank's documentation pages. It runs from outside the suite: no file of mbTest is
patched, so moving to a new Mountebank release is a one-line change to `MBTEST_REF` (which
`scripts/verify-mountebank-version-docs.sh` holds to `MB_VERSION` in `ci.yml`).

## Running it

```bash
npm install --prefix ~/bench-mb @mbtest/mountebank@2.9.5   # once; the baseline engine
cargo build -p rift-http-proxy --bin rift-http-proxy
RIFT_BIN=$PWD/target/debug/rift-http-proxy conformance/mbtest/run.sh
```

`run.sh` clones Mountebank at `MBTEST_REF` (`--depth 1`) into `$RUNNER_TEMP` or `$TMPDIR`, runs
`npm ci --ignore-scripts` in `mbTest/` and then runs every phase against **Mountebank first** (the
baseline) and then **Rift**. It needs node 22 or later, `git`, `curl` and `python3`.

| Variable | Meaning |
|:--|:--|
| `RIFT_BIN` | The `rift-http-proxy` binary under test. |
| `MB_BIN` | Mountebank's `mb` (default `~/bench-mb/node_modules/.bin/mb`). It is the baseline and serves the docs snapshot. |
| `MBTEST_CHECKOUT` | Reuse a checkout of `MBTEST_REF` that already has `npm ci` done in `mbTest/`. |
| `OUT_DIR` | Where results go (default `conformance/mbtest/out/`, ignored by git). |

`run.sh rift` or `run.sh mountebank` runs one engine. mbTest binds fixed ports (2525-2527, 3000,
4545 and others), so nothing else may listen on them; `run.sh` refuses to start when one is taken,
and runs the phases one at a time.

**The three phases.**

| Phase | What runs | The engine |
|:--|:--|:--|
| `api` | `api/homeControllerTest.js`, `api/impostersControllerTest.js`, `api/http/*`, `api/https/*` | One engine on 2525, `--allowInjection --localOnly` (as mbTest's `tasks/mbtest.js` starts it). Two tests spawn their own through `MB_EXECUTABLE`. |
| `cli` | `cli/*` | None: every test spawns `MB_EXECUTABLE` itself. |
| `docs` | [`docsScenarios.js`](docsScenarios.js): every scenario on the 14 pages `web/docsIntegrityTest.js` checks, one test each | One engine on 2525; the pages from a snapshot on 2600. |

For Rift, `MB_EXECUTABLE` is [`rift-as-mb`](rift-as-mb), which makes exactly two translations:
`--logfile` becomes `--log` (until #1559 adds the alias), and every start gets its own
`--metrics-port`, since Rift binds a second listener that Mountebank has no counterpart for. It
translates nothing else and never hides a failure: a flag Rift refuses, or a `rift stop` that
exits 1, is a finding.

**The docs snapshot.** Rift serves no HTML, so [`docs-snapshot.sh`](docs-snapshot.sh) starts
Mountebank on 2525, saves the 14 pages, and stops it; [`snapserve.py`](snapserve.py) serves them on
2600 (`MB_PORT=2600` for the docs tester). The pages are rendered on 2525 because every admin
example in them prints `Host: localhost:<the serving port>`. The snapshot is taken on every run,
never committed, so it cannot go stale.

**The docs normaliser.** `docsScenarios.js` runs mbTest's own `docs.getScenarios()` and compares
each step exactly as `docs.js` does, except that a printed HTTP response is compared as a message:
`x-rift-*` markers, `Content-Length`/`Transfer-Encoding`/`Connection` are dropped, header names are
folded to lower case and the header lines sorted. Each rule cites the documented difference or the
filed bug it stands for. It never touches a status line or a body, and it leaves Mountebank's own
results unchanged.

## Results and the expected list

Each engine's mocha JSON (`api.json`, `cli.json`, `docs.json`) and engine logs land in
`OUT_DIR/<engine>/`. [`check.py`](check.py) turns them into `OUT_DIR/<engine>/results.json` and
holds them against [`expected.json`](expected.json), printing the counts (and appending them to the
job summary in CI). A test id is `<file under mbTest>::<mocha full title>` (so the http and https
variants stay distinct), or `docs:<page>#<scenario>`. `run.sh` first lists every test in the
suite with `mocha --dry-run`, so a test that never ran (a failing hook aborted its file) counts as a
failure, not as nothing.

The run fails when:

- a Rift test fails, or never runs, and `expected.json` does not list it;
- a test `expected.json` lists **passes** on Rift: remove the entry. The list only shrinks;
- a Mountebank test fails and is not listed as `harness`;
- an id in `expected.json` names no test in the suite (renamed upstream), or a `*Test.js` file is
  neither run nor listed as unsupported;
- an entry is malformed, or a `doc` anchor it cites does not exist.

`python3 conformance/mbtest/check.py --self-test` proves each of those rules on planted inputs.

### Classifying a failure

Every entry in `expected` has a `class`:

| Class | Means | Needs |
|:--|:--|:--|
| `bug` | Rift should behave as Mountebank does. | `issue`: the filed issue, `#<number>`. One issue per root cause, not per test: the http and https variants of a test share one. |
| `documented` | A deliberate difference. | `doc`: an anchor in `docs/mountebank/differences.md` that exists. A difference that is not on that page is a bug. |
| `harness` | The test cannot pass against any engine here. | `reason`. It must fail (or be skipped) on Mountebank too; `check.py` checks this against the Mountebank run, unless the entry says `"onMountebank": "passes"` and its reason explains why no foreign binary can pass it. |

A `flaky` string marks an entry whose outcome varies from run to run (a random key order, a
public website, the host's name resolution) and says why: it is exempt from the stale-entry rule
and, for `harness`, from the Mountebank check. A `note` says what the test showed. When a fix lands, the test passes, the run fails on the stale
entry, and the entry is deleted in the same change, as with the differential allow-list. A test
that fails for more than one reason is listed under the first one it hits.

`unsupported` excludes tests by glob (`api/tcp/**`, or a full id) and counts them in the report:
the `tcp` and `smtp` tests (Rift imposters are HTTP/HTTPS only), `js/` and `perf/` (they load
Mountebank's own sources in-process), and `web/` tests that read Mountebank's HTML site.
`web/docsIntegrityTest.js` is replaced by `docsScenarios.js`. A glob without `::` names test files.

## CI

[`.github/workflows/mbtest.yml`](../../.github/workflows/mbtest.yml) runs weekly (Mondays 05:41
UTC) and on demand (`workflow_dispatch`, with an optional `ref`), against a debug build. It is not
a pull-request check yet: the list will move while the first findings are fixed. It uploads
`OUT_DIR` as the `mbtest-results` artifact.
