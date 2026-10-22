//! Generative differential testing (issue #1549): random imposters and admin/request sequences run
//! on Mountebank and on Rift, and any unexplained difference is shrunk to a minimal case.
//!
//! The hand-written cases (`tests/differential.rs`) only reach the sequences someone thought of.
//! Here the model test's generators (`crates/rift-mock-core/tests/model/ops.rs`) draw stubs and
//! requests, `generative/vocab.rs` keeps the part Mountebank can run and adds the operations it has
//! routes for, and `rift_differential::driver::run_case` compares the two engines exactly as it does
//! for a committed case. A failure is shrunk by proptest, re-run three times, and written to
//! `<target>/generative/<seed>.json` as a case file ready for `cases/generated.json`.
//!
//! Environment (see `generative/budget.rs`): `RIFT_GEN_SEED`, `RIFT_GEN_CASES`,
//! `RIFT_GEN_SECONDS`, `RIFT_GEN_MAX_STEPS_PER_SEC`, and `RIFT_GEN_OUT` for the output directory.
//! Skipped without Mountebank unless `RIFT_DIFFERENTIAL_REQUIRE_MB` is set, as the committed suite.

#[path = "../../../crates/rift-mock-core/tests/model/ops.rs"]
#[allow(dead_code)]
mod ops;

#[path = "generative/budget.rs"]
mod budget;
#[path = "generative/emit.rs"]
mod emit;
#[path = "generative/verdict.rs"]
mod verdict;
#[path = "generative/vocab.rs"]
mod vocab;

use budget::{Budget, Pacer};
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner};
use rift_differential::allow::{self, Rule};
use rift_differential::case::Case;
use rift_differential::driver::{Engines, run_case};
use rift_differential::engine::{self, client};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use verdict::Verdict;

/// Joins a failing case's locations in proptest's failure reason (no location holds a newline).
const LOCATION_SEPARATOR: &str = "\n";

/// How often a shrunk divergence is re-run before it is reported as one.
const CONFIRMATIONS: usize = 3;

fn harness_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_allow_list() -> Vec<Rule> {
    allow::load(&harness_dir().join("allowlist.json")).unwrap_or_else(|e| panic!("{e}"))
}

/// `RIFT_GEN_OUT`, else `generative/` in the target directory this test binary was built into.
fn out_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RIFT_GEN_OUT") {
        return PathBuf::from(dir);
    }
    // target/<profile>/deps/<test binary> → target/generative
    let exe = std::env::current_exe().unwrap_or_else(|e| panic!("current_exe: {e}"));
    exe.ancestors()
        .nth(3)
        .map(|target| target.join("generative"))
        .unwrap_or_else(|| panic!("{} is not in a target directory", exe.display()))
}

fn parse_case(value: &Value) -> Case {
    serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("the emitter wrote a case that does not parse: {e}\n{value}"))
}

/// What a batch ended with.
enum Outcome {
    /// Every case agreed.
    Clean,
    /// A divergence, shrunk: the case JSON and its unexplained locations.
    Diverged(Value, BTreeSet<String>),
    /// The run broke; see [`Verdict::Infrastructure`].
    Broken(Vec<String>),
}

struct Checked {
    verdict: Verdict,
    steps: usize,
    detail: String,
}

struct Run<'a> {
    rt: &'a tokio::runtime::Runtime,
    client: &'a reqwest::Client,
    engines: &'a Engines,
    rules: &'a [Rule],
}

impl Run<'_> {
    /// Runs `case` on both engines: its verdict, how many steps it sent (for the pacer), and the
    /// unexplained differences with both engines' values, for the report.
    fn check(&self, case: &Value) -> Checked {
        let parsed = parse_case(case);
        let report = self
            .rt
            .block_on(run_case(self.client, self.engines, &parsed));
        let mut used = vec![0usize; self.rules.len()];
        let unexplained = allow::classify(&report.name, &report.differences, self.rules, &mut used);
        let mut detail = String::new();
        for diff in &unexplained {
            let _ = writeln!(detail, "  {diff}");
        }
        // A stored imposter's unified diff, only where an unexplained difference lies in it: the
        // allow-listed ones (`recordMatches`, …) are noise in a report.
        for stored in &report.stored_diffs {
            let at = stored
                .split_once(":\n")
                .map_or(stored.as_str(), |(at, _)| at);
            if unexplained.iter().any(|d| d.location.starts_with(at)) {
                let _ = writeln!(detail, "  {}", stored.replace('\n', "\n  "));
            }
        }
        Checked {
            verdict: verdict::of(&report, self.rules),
            // The steps, plus the stored comparison and cleanup run_case adds.
            steps: parsed.steps.len() + 6,
            detail,
        }
    }

    /// One proptest batch of `cases` cases from `seed`. Stops generating once `deadline` passes,
    /// but always finishes a shrink it has started.
    fn batch(
        &self,
        seed: u64,
        cases: u32,
        deadline: Option<Instant>,
        pacer: &RefCell<Pacer>,
    ) -> Outcome {
        let config = Config {
            cases,
            failure_persistence: None,
            max_shrink_iters: 2048,
            max_shrink_time: 900_000,
            ..Config::default()
        };
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        let mut runner =
            TestRunner::new_with_rng(config, TestRng::from_seed(RngAlgorithm::ChaCha, &bytes));
        let broken: RefCell<Option<Vec<String>>> = RefCell::new(None);
        let shrinking = RefCell::new(false);
        let name = format!("generated: seed {seed}");
        let result = runner.run(&vocab::case(), |(initial, ops)| {
            let late = deadline.is_some_and(|d| Instant::now() >= d) && !*shrinking.borrow();
            if broken.borrow().is_some() || late {
                return Ok(());
            }
            let case = emit::case(&name, &initial, &ops);
            let Checked { verdict, steps, .. } = self.check(&case);
            let wait = pacer.borrow_mut().record(steps);
            std::thread::sleep(wait);
            match verdict {
                Verdict::Agree => Ok(()),
                // Passing from here on stops the shrink at the last real failure, and no
                // infrastructure error is ever shrunk as if it were one.
                Verdict::Infrastructure(why) => {
                    *broken.borrow_mut() = Some(why);
                    Ok(())
                }
                Verdict::Diverge(locations) => {
                    *shrinking.borrow_mut() = true;
                    Err(TestCaseError::fail(
                        locations
                            .into_iter()
                            .collect::<Vec<_>>()
                            .join(LOCATION_SEPARATOR),
                    ))
                }
            }
        });
        // A broken run is not trusted for anything, a shrink it interrupted included.
        if let Some(why) = broken.into_inner() {
            return Outcome::Broken(why);
        }
        match result {
            // The reason carries the minimal case's unexplained locations; the caller re-runs it
            // to tell a divergence from a flake.
            Err(TestError::Fail(reason, (initial, ops))) => Outcome::Diverged(
                emit::case(&name, &initial, &ops),
                reason
                    .message()
                    .split(LOCATION_SEPARATOR)
                    .map(String::from)
                    .collect(),
            ),
            Err(TestError::Abort(why)) => Outcome::Broken(vec![format!("proptest aborted: {why}")]),
            Ok(()) => Outcome::Clean,
        }
    }
}

/// Writes `file` under the output directory and returns its path.
fn write(file: &str, text: &str) -> PathBuf {
    let dir = out_dir();
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    let path = dir.join(file);
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    path
}

#[test]
fn generated_sequences_match_mountebank() {
    let Some(mb) = engine::locate_mountebank() else {
        assert!(
            std::env::var_os("RIFT_DIFFERENTIAL_REQUIRE_MB").is_none(),
            "RIFT_DIFFERENTIAL_REQUIRE_MB is set but no Mountebank was found"
        );
        eprintln!("SKIP generated_sequences_match_mountebank: no Mountebank found");
        return;
    };
    let budget = Budget::from(|name| std::env::var(name).ok()).unwrap_or_else(|e| panic!("{e}"));
    // A verdict left by an earlier run would be filed as this one's.
    let stale = out_dir().join("verdict.json");
    match std::fs::remove_file(&stale) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("{}: {e}", stale.display()),
    }
    let rift_bin = engine::locate_rift().unwrap_or_else(|e| panic!("{e}"));
    let cwd = harness_dir()
        .join("../../sdk-conformance/corpus")
        .canonicalize()
        .expect("corpus dir");
    let scripts_dir = harness_dir()
        .join("../..")
        .canonicalize()
        .expect("repo root");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut engines = rt.block_on(async {
        Engines {
            // Unprofiled: the coverage ratchet measures the committed cases only.
            mountebank: engine::start_mountebank_profiled(&mb, &cwd, None)
                .await
                .unwrap_or_else(|e| panic!("{e}")),
            rift: engine::start_rift(&rift_bin, &cwd, &scripts_dir)
                .await
                .unwrap_or_else(|e| panic!("{e}")),
            cwd: cwd.clone(),
        }
    });
    let client = client().expect("http client");
    let rules = load_allow_list();
    let run = Run {
        rt: &rt,
        client: &client,
        engines: &engines,
        rules: &rules,
    };

    let started = Instant::now();
    let deadline = budget.seconds.map(|s| started + Duration::from_secs(s));
    let pacer = RefCell::new(Pacer::new(budget.max_steps_per_sec));
    let mut batches = 0u64;
    let (outcome, seed) = loop {
        let seed = budget.seed.wrapping_add(batches);
        batches += 1;
        let outcome = run.batch(seed, budget.cases, deadline, &pacer);
        let more = deadline.is_some_and(|d| Instant::now() < d);
        if !matches!(outcome, Outcome::Clean) || !more {
            break (outcome, seed);
        }
    };
    eprintln!(
        "generative: seed {} ({batches} batch(es) of {} cases), {} steps in {:.1}s, Rift {}",
        budget.seed,
        budget.cases,
        pacer.borrow().steps(),
        started.elapsed().as_secs_f64(),
        rift_bin.display()
    );

    // Re-run a shrunk divergence before anything is stopped: it is reported only when every re-run
    // fails at the same locations.
    let (outcome, confirmed, detail) = match outcome {
        Outcome::Diverged(case, locations) => {
            let reruns: Vec<Checked> = (0..CONFIRMATIONS).map(|_| run.check(&case)).collect();
            let broken: Vec<String> = reruns
                .iter()
                .filter_map(|c| match &c.verdict {
                    Verdict::Infrastructure(why) => Some(why.clone()),
                    Verdict::Agree | Verdict::Diverge(_) => None,
                })
                .flatten()
                .collect();
            let same = reruns
                .iter()
                .all(|c| c.verdict == Verdict::Diverge(locations.clone()));
            let detail = reruns.into_iter().map(|c| c.detail).find(|d| !d.is_empty());
            if broken.is_empty() {
                (
                    Outcome::Diverged(case, locations),
                    same,
                    detail.unwrap_or_default(),
                )
            } else {
                (Outcome::Broken(broken), false, String::new())
            }
        }
        other => (other, false, String::new()),
    };
    let stopped: Vec<String> = [&mut engines.mountebank, &mut engines.rift]
        .into_iter()
        .filter_map(|engine| rt.block_on(engine.stop()).err())
        .collect();

    match outcome {
        Outcome::Clean => {}
        Outcome::Broken(why) => {
            let path = write(
                "verdict.json",
                &json!({ "kind": "infrastructure", "seed": seed, "errors": why }).to_string(),
            );
            panic!(
                "generative: the run broke at seed {seed}; this is not a divergence and was not \
                 shrunk ({}):\n  {}",
                path.display(),
                why.join("\n  ")
            );
        }
        Outcome::Diverged(mut case, locations) => {
            let kind = if confirmed { "divergence" } else { "flaky" };
            let sig = verdict::signature(&locations);
            case["name"] = json!(format!("generated: {sig}"));
            let file = json!({
                "source": format!("generative seed {seed} (#1549)"),
                "cases": [case],
            });
            let text = serde_json::to_string_pretty(&file).expect("a case file serialises");
            let path = write(&format!("{seed}.json"), &format!("{text}\n"));
            write(
                "verdict.json",
                &json!({
                    "kind": kind,
                    "seed": seed,
                    "sig": sig,
                    "case": path.display().to_string(),
                    "locations": locations,
                    "differences": detail,
                })
                .to_string(),
            );
            panic!(
                "generative: Rift and Mountebank answered differently ({kind}, gen-sig:{sig}). \
                 Reproduce with RIFT_GEN_SEED={seed} RIFT_GEN_CASES={}. Unexplained at:\n  {}\n\
                 Mountebank against Rift:\n{detail}\nShrunk case ({}):\n{text}",
                budget.cases,
                locations.into_iter().collect::<Vec<_>>().join("\n  "),
                path.display()
            );
        }
    }
    assert!(stopped.is_empty(), "stopping the engines: {stopped:?}");
}

mod unit {
    use super::*;
    use crate::ops::{Method, Mode, Pred, Shape};
    use rift_differential::diff::{DiffClass, Difference};
    use rift_differential::driver::CaseReport;
    use vocab::{Decoration, MbEdit, MbOp, MbSpec, Operator};

    fn plain() -> Decoration {
        Decoration {
            operator: Operator::Equals,
            repeat: 1,
            copy: None,
            wait: false,
        }
    }

    fn is(path: &'static str) -> MbSpec {
        MbSpec::Is {
            pred: Pred {
                path: Some(path),
                ..Pred::default()
            },
            label: "p",
            responses: 1,
            decoration: plain(),
        }
    }

    #[test]
    fn the_emitter_writes_a_case_that_parses() {
        let request = Shape {
            method: Method::Post,
            path: "/a",
            query: true,
            body: Some("x"),
            flow: "f1",
        };
        let ops = [
            MbOp::Request(request),
            MbOp::AddStub {
                index: 7,
                spec: is("/b"),
            },
            MbOp::DeleteStub(5),
            MbOp::Recreate(vec![MbEdit::Swap(0, 1)]),
        ];
        let case = emit::case("generated: unit", &[is("/a")], &ops);
        let steps: Vec<(String, String)> = case["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .map(|s| {
                let what = s["method"].as_str().unwrap_or_default().to_string();
                (what, s["path"].as_str().unwrap_or_default().to_string())
            })
            .collect();
        let expected = [
            ("POST", "/imposters"),
            ("POST", "/imposters"),
            ("POST", "/a?q=1"),
            // index 7 % (1 + 1) = 1: appended after the one authored stub.
            ("POST", "/imposters/4545/stubs"),
            ("GET", "/imposters/4545"),
            // 5 % (2 + 1) = 2: one past the end, which both engines must refuse alike.
            ("DELETE", "/imposters/4545/stubs/2"),
            ("GET", "/imposters/4545"),
            ("DELETE", "/imposters/4545"),
            ("POST", "/imposters"),
            ("GET", "/imposters/4545"),
        ];
        let expected: Vec<(String, String)> = expected
            .iter()
            .map(|(m, p)| ((*m).to_string(), (*p).to_string()))
            .collect();
        assert_eq!(steps, expected);
        assert_eq!(case["steps"][2]["body"], json!("x"));
        assert_eq!(case["steps"][2].get("headers"), None, "no X-Flow header");
        assert_eq!(case["steps"][3]["body"]["index"], json!(1));
        // The recreated imposter carries the swapped list.
        let recreated = &case["steps"][8]["body"]["stubs"];
        assert_eq!(recreated[0]["predicates"][0]["equals"]["path"], json!("/b"));
        assert_eq!(recreated[1]["predicates"][0]["equals"]["path"], json!("/a"));
        let parsed = parse_case(&case);
        assert_eq!(parsed.steps.len(), 10);
        assert_eq!(
            parsed.logical_ports().into_iter().collect::<Vec<_>>(),
            vec![vocab::IMPOSTER, vocab::UPSTREAM]
        );
    }

    #[test]
    fn decoration_and_proxies_are_written_as_mountebank_reads_them() {
        let stub = MbSpec::Is {
            pred: Pred {
                method: Some(Method::Get),
                query: true,
                ..Pred::default()
            },
            label: "q",
            responses: 2,
            decoration: Decoration {
                operator: Operator::Matches,
                repeat: 3,
                copy: Some(vocab::CopyFrom::Query),
                wait: true,
            },
        }
        .to_json();
        assert_eq!(
            stub["predicates"],
            json!([{ "matches": { "method": "^GET$", "query": { "q": "^1$" } } }])
        );
        assert_eq!(stub["responses"][0]["repeat"], json!(3));
        assert_eq!(stub["responses"][1].get("repeat"), None);
        assert_eq!(stub["responses"][1]["is"]["body"], json!("q1:${C}"));
        assert_eq!(
            stub["responses"][0]["behaviors"],
            json!([
                { "copy": { "from": { "query": "q" }, "into": "${C}",
                            "using": { "method": "regex", "selector": ".*" } } },
                { "wait": 0 }
            ])
        );

        let proxy = MbSpec::Proxy {
            pred: Pred::default(),
            operator: Operator::Not,
            mode: Mode::Always,
            generator: vocab::Generator::Body,
        }
        .to_json();
        assert_eq!(proxy["predicates"], json!([]), "no fields, no predicate");
        assert_eq!(
            proxy["responses"][0]["proxy"],
            json!({ "to": "http://localhost:4546", "mode": "proxyAlways",
                    "predicateGenerators": [{ "matches": { "body": true } }] })
        );
    }

    #[test]
    fn every_exclusion_cites_an_excuse_that_still_exists() {
        let rules = load_allow_list();
        let ids: BTreeSet<&str> = rules.iter().map(|r| r.entry.id.as_str()).collect();
        let root = harness_dir().join("../..");
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        for (shape, excuse) in vocab::EXCLUDED {
            match excuse {
                vocab::Excuse::AllowList(id) => {
                    assert!(ids.contains(id), "{shape}: allow-list id {id:?} is gone");
                }
                vocab::Excuse::Doc { file, quote } => {
                    let text = std::fs::read_to_string(root.join(file))
                        .unwrap_or_else(|e| panic!("{shape}: {file}: {e}"));
                    assert!(
                        squash(&text).contains(&squash(quote)),
                        "{shape}: {file} no longer says {quote:?}"
                    );
                }
                vocab::Excuse::Choice(why) => assert!(!why.trim().is_empty(), "{shape}"),
            }
        }
    }

    #[test]
    fn the_signature_ignores_indices() {
        let one: BTreeSet<String> = ["stored[4545]/stubs/3/x", "step[12].status"]
            .map(String::from)
            .into();
        let other: BTreeSet<String> = ["stored[4545]/stubs/1/x", "step[2].status"]
            .map(String::from)
            .into();
        assert_eq!(verdict::signature(&one), verdict::signature(&other));
        assert_eq!(
            verdict::shape("step[3].rep[1].body/p0"),
            "step[*].rep[*].body/p0"
        );
        assert_eq!(verdict::shape("step[3].header.x-a1"), "step[*].header.x-a1");
        let elsewhere: BTreeSet<String> = ["stored[4545]/stubs/1/y"].map(String::from).into();
        assert_ne!(verdict::signature(&one), verdict::signature(&elsewhere));
        assert_eq!(verdict::signature(&one).len(), 8);
    }

    #[test]
    fn an_infrastructure_error_is_never_a_divergence() {
        let rules = load_allow_list();
        let difference = Difference {
            location: "step[2].status".to_string(),
            class: DiffClass::Value,
            mountebank: "200".to_string(),
            rift: "404".to_string(),
        };
        let mut report = CaseReport {
            name: "generated: unit".to_string(),
            differences: vec![difference.clone()],
            ..CaseReport::default()
        };
        assert_eq!(
            verdict::of(&report, &rules),
            Verdict::Diverge(["step[2].status".to_string()].into())
        );
        report
            .errors
            .push("port allocation: os error 49".to_string());
        assert!(matches!(
            verdict::of(&report, &rules),
            Verdict::Infrastructure(_)
        ));
        let unreachable = CaseReport {
            name: "generated: unit".to_string(),
            unreachable: vec!["step[4]".to_string()],
            ..CaseReport::default()
        };
        assert!(matches!(
            verdict::of(&unreachable, &rules),
            Verdict::Infrastructure(_)
        ));
        let clean = CaseReport {
            name: "generated: unit".to_string(),
            ..CaseReport::default()
        };
        assert_eq!(verdict::of(&clean, &rules), Verdict::Agree);
    }

    #[test]
    fn the_budget_reads_the_environment_and_refuses_typos() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        assert_eq!(
            Budget::from(env(&[])),
            Ok(Budget {
                seed: budget::DEFAULT_SEED,
                cases: budget::DEFAULT_CASES,
                seconds: None,
                max_steps_per_sec: budget::DEFAULT_MAX_STEPS_PER_SEC,
            })
        );
        let nightly = Budget::from(env(&[
            ("RIFT_GEN_SEED", "99"),
            ("RIFT_GEN_SECONDS", "1200"),
        ]))
        .expect("valid");
        assert_eq!((nightly.seed, nightly.seconds), (99, Some(1200)));
        assert!(Budget::from(env(&[("RIFT_GEN_CASES", "0")])).is_err());
        assert!(Budget::from(env(&[("RIFT_GEN_SECONDS", "20m")])).is_err());
        assert!(Budget::from(env(&[("RIFT_GEN_SEED", "x")])).is_err());

        let mut pacer = Pacer::new(100);
        assert!(
            pacer.record(50) > Duration::from_millis(400),
            "50 steps at 100/s is 0.5 s"
        );
    }
}
