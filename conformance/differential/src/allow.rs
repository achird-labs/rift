//! The known-divergence list (`allowlist.json`).
//!
//! An entry permits one kind of difference and says why. It is either
//!
//! - `documented`: a deliberate deviation, citing a doc file and a verbatim quote from it. The
//!   harness checks the quote is still there, so deleting the doc line turns the harness red; or
//! - `known-bug`: a Rift divergence that is not intended, with the reason (and issue, once filed).
//!
//! What it matches:
//!
//! - `case`: one case name, or `*`;
//! - `location`: a glob over the difference location (`*` matches any run of characters), which
//!   also covers everything below it (`step[1].body` covers `step[1].body/a/0`);
//! - `mountebank` / `rift`: the exact rendered value on that side, or `mountebankMatches` /
//!   `riftMatches`: an anchored regex over it. A JSON value renders as JSON (`200`, `"200"`, `{}`),
//!   a header as its text, a missing one as `(absent)`.
//!
//! - `relation`: a tie between the two values. `rift-quotes-mountebank` means Rift's value is
//!   Mountebank's wrapped in quotes (`200` against `"200"`): two independent regexes could not say
//!   that both name the same status code.
//!
//! A `*` case is allowed only when both sides are pinned (exact, regex, or a relation) — a precise
//! claim about what Rift does differently everywhere, never a blanket ignore of a field — or for the
//! `json-text` class, which is itself precise (see `diff::DiffClass`). A difference is credited to
//! the first entry that matches it, so an entry wholly shadowed by an earlier one is stale too. An
//! entry that matches nothing in a full run is stale and fails the run, so a fixed divergence cannot
//! leave its excuse behind.

use crate::diff::{DiffClass, Difference};
use regex::Regex;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub case: String,
    pub location: String,
    #[serde(default = "value_class")]
    pub class: DiffClass,
    #[serde(default)]
    pub mountebank: Option<String>,
    #[serde(default)]
    pub rift: Option<String>,
    #[serde(default)]
    pub mountebank_matches: Option<String>,
    #[serde(default)]
    pub rift_matches: Option<String>,
    #[serde(default)]
    pub relation: Option<Relation>,
    pub kind: Kind,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default)]
    pub quote: Option<String>,
    #[serde(default)]
    pub issue: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Documented,
    KnownBug,
}

/// A tie between the two sides' values (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Relation {
    RiftQuotesMountebank,
}

impl Relation {
    fn holds(self, mountebank: &str, rift: &str) -> bool {
        match self {
            Self::RiftQuotesMountebank => rift
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .is_some_and(|inner| inner == mountebank),
        }
    }
}

fn value_class() -> DiffClass {
    DiffClass::Value
}

/// A validated entry with its matchers compiled.
#[derive(Debug, Clone)]
pub struct Rule {
    pub entry: Entry,
    location: Regex,
    mountebank: Option<Regex>,
    rift: Option<Regex>,
}

impl Rule {
    /// Validates and compiles one entry.
    ///
    /// # Errors
    /// A bad regex, a `*` case without both sides pinned (outside `json-text`), a value given
    /// both exactly and as a regex, a `documented` entry without `doc` and `quote`, or an empty
    /// `reason`.
    pub fn new(entry: Entry) -> Result<Self, String> {
        let id = &entry.id;
        let side =
            |exact: &Option<String>, pattern: &Option<String>, name: &str| match (exact, pattern) {
                (Some(_), Some(_)) => {
                    Err(format!("{id}: {name} given both exactly and as a regex"))
                }
                (Some(value), None) => {
                    compile(&format!("^{}$", regex::escape(value)), id).map(Some)
                }
                (None, Some(pattern)) => compile(&format!("^(?:{pattern})$"), id).map(Some),
                (None, None) => Ok(None),
            };
        let mountebank = side(&entry.mountebank, &entry.mountebank_matches, "mountebank")?;
        let rift = side(&entry.rift, &entry.rift_matches, "rift")?;
        let pinned = (mountebank.is_some() && rift.is_some()) || entry.relation.is_some();
        if entry.case == "*" && !pinned && entry.class != DiffClass::JsonText {
            return Err(format!(
                "{id}: a `*` case must pin both the mountebank and the rift value"
            ));
        }
        if entry.kind == Kind::Documented && (entry.doc.is_none() || entry.quote.is_none()) {
            return Err(format!("{id}: a documented entry needs doc and quote"));
        }
        if entry.reason.trim().is_empty() {
            return Err(format!("{id}: reason is empty"));
        }
        let glob = entry
            .location
            .split('*')
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(".*");
        let location = compile(&format!("^{glob}(?:[/.].*)?$"), id)?;
        Ok(Self {
            entry,
            location,
            mountebank,
            rift,
        })
    }

    #[must_use]
    pub fn matches(&self, case: &str, diff: &Difference) -> bool {
        self.entry.class == diff.class
            && (self.entry.case == "*" || self.entry.case == case)
            && self.location.is_match(&diff.location)
            && self
                .mountebank
                .as_ref()
                .is_none_or(|re| re.is_match(&diff.mountebank))
            && self.rift.as_ref().is_none_or(|re| re.is_match(&diff.rift))
            && self
                .entry
                .relation
                .is_none_or(|relation| relation.holds(&diff.mountebank, &diff.rift))
    }
}

fn compile(pattern: &str, id: &str) -> Result<Regex, String> {
    Regex::new(pattern).map_err(|e| format!("{id}: {e}"))
}

/// Credits each of `differences` to the first rule that matches it (counting into `used`, one slot
/// per rule) and returns the ones no rule explains.
pub fn classify<'d>(
    case: &str,
    differences: &'d [Difference],
    rules: &[Rule],
    used: &mut [usize],
) -> Vec<&'d Difference> {
    differences
        .iter()
        .filter(
            |diff| match rules.iter().position(|r| r.matches(case, diff)) {
                Some(index) => {
                    used[index] += 1;
                    false
                }
                None => true,
            },
        )
        .collect()
}

/// The ids of the rules that explained nothing.
#[must_use]
pub fn stale<'r>(rules: &'r [Rule], used: &[usize]) -> Vec<&'r str> {
    rules
        .iter()
        .zip(used)
        .filter(|(_, n)| **n == 0)
        .map(|(r, _)| r.entry.id.as_str())
        .collect()
}

/// Loads and validates the allow-list.
///
/// # Errors
/// Malformed JSON, a duplicate id, or any [`Rule::new`] error.
pub fn load(path: &Path) -> Result<Vec<Rule>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let entries: Vec<Entry> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut ids = std::collections::BTreeSet::new();
    for entry in &entries {
        if !ids.insert(entry.id.clone()) {
            return Err(format!("duplicate allow-list id {:?}", entry.id));
        }
    }
    entries.into_iter().map(Rule::new).collect()
}

/// Checks every `documented` entry's quote is present in its doc file (whitespace-insensitive).
///
/// # Errors
/// Lists each entry whose doc is missing or no longer contains the quote.
pub fn check_citations(rules: &[Rule], repo_root: &Path) -> Result<(), String> {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut broken = Vec::new();
    for entry in rules
        .iter()
        .map(|r| &r.entry)
        .filter(|e| e.kind == Kind::Documented)
    {
        let (Some(doc), Some(quote)) = (&entry.doc, &entry.quote) else {
            broken.push(format!("{}: missing doc/quote", entry.id));
            continue;
        };
        match std::fs::read_to_string(repo_root.join(doc)) {
            Ok(text) if squash(&text).contains(&squash(quote)) => {}
            Ok(_) => broken.push(format!("{}: {doc} no longer says {quote:?}", entry.id)),
            Err(e) => broken.push(format!("{}: {doc}: {e}", entry.id)),
        }
    }
    if broken.is_empty() {
        Ok(())
    } else {
        Err(broken.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(case: &str, location: &str) -> Entry {
        Entry {
            id: "e".to_string(),
            case: case.to_string(),
            location: location.to_string(),
            class: DiffClass::Value,
            mountebank: None,
            rift: None,
            mountebank_matches: None,
            rift_matches: None,
            relation: None,
            kind: Kind::KnownBug,
            doc: None,
            quote: None,
            issue: None,
            reason: "r".to_string(),
        }
    }

    fn diff(location: &str, mountebank: &str, rift: &str) -> Difference {
        Difference {
            location: location.to_string(),
            class: DiffClass::Value,
            mountebank: mountebank.to_string(),
            rift: rift.to_string(),
        }
    }

    #[test]
    fn an_entry_matches_only_its_own_class() {
        let rule = Rule::new(entry("c", "step[1].body")).expect("valid");
        let mut text = diff("step[1].body", "a", "b");
        text.class = DiffClass::JsonText;
        assert!(!rule.matches("c", &text));
    }

    #[test]
    fn a_relation_ties_the_two_values_together() {
        let mut e = entry("*", "*/is/statusCode");
        e.relation = Some(Relation::RiftQuotesMountebank);
        let rule = Rule::new(e).expect("a relation pins both sides");
        let at = "stored[4545]/stubs/0/responses/0/is/statusCode";
        assert!(rule.matches("any", &diff(at, "201", "\"201\"")));
        assert!(!rule.matches("any", &diff(at, "201", "\"200\"")));
        assert!(!rule.matches("any", &diff(at, "201", "201")));
    }

    #[test]
    fn classify_credits_the_first_matching_rule_and_stale_names_the_rest() {
        let rules = vec![
            Rule::new(entry("c", "step[0]")).expect("valid"),
            Rule::new(Entry {
                id: "shadowed".to_string(),
                ..entry("c", "step[0].body")
            })
            .expect("valid"),
            Rule::new(Entry {
                id: "unused".to_string(),
                ..entry("c", "step[9]")
            })
            .expect("valid"),
        ];
        let diffs = vec![
            diff("step[0].body", "a", "b"),
            diff("step[1].status", "200", "404"),
        ];
        let mut used = vec![0; rules.len()];
        let left = classify("c", &diffs, &rules, &mut used);
        assert_eq!(left, vec![&diffs[1]]);
        assert_eq!(used, vec![1, 0, 0]);
        assert_eq!(stale(&rules, &used), vec!["shadowed", "unused"]);
    }

    #[test]
    fn load_refuses_duplicate_ids_and_unknown_keys() {
        let dir =
            std::env::temp_dir().join(format!("rift-differential-allow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let write = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, body).expect("write allow-list");
            path
        };
        let one = r#"{"id": "x", "case": "c", "location": "step[0]", "kind": "known-bug", "reason": "r"}"#;
        assert!(load(&write("ok.json", &format!("[{one}]"))).is_ok());
        let duplicate = load(&write("dup.json", &format!("[{one}, {one}]")));
        assert!(duplicate.expect_err("duplicate id").contains("duplicate"));
        let typo = one.replace(r#""reason""#, r#""reasn""#);
        assert!(load(&write("typo.json", &format!("[{typo}]"))).is_err());
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn a_case_entry_matches_its_location_and_what_lies_below_it() {
        let rule = Rule::new(entry("c", "step[1].body")).expect("valid");
        assert!(rule.matches("c", &diff("step[1].body", "a", "b")));
        assert!(rule.matches("c", &diff("step[1].body/a/0", "a", "b")));
        assert!(!rule.matches("c", &diff("step[1].bodyx", "a", "b")));
        assert!(!rule.matches("c", &diff("step[11].body", "a", "b")));
        assert!(!rule.matches("other", &diff("step[1].body", "a", "b")));
    }

    #[test]
    fn a_glob_location_with_pinned_values_matches_only_those_values() {
        let mut e = entry("*", "*/stubs/*/responses/*/is/statusCode");
        e.mountebank_matches = Some(r"\d{3}".to_string());
        e.rift_matches = Some(r#""\d{3}""#.to_string());
        let rule = Rule::new(e).expect("valid");
        let at = "stored[4545]/stubs/0/responses/1/is/statusCode";
        assert!(rule.matches("any", &diff(at, "200", "\"200\"")));
        assert!(!rule.matches("any", &diff(at, "200", "201")));
        assert!(!rule.matches("any", &diff("stored[4545]/stubs/0/name", "200", "\"200\"")));

        let mut exact = entry("*", "step[*].header.x-rift-imposter");
        exact.mountebank = Some("(absent)".to_string());
        exact.rift = Some("true".to_string());
        let rule = Rule::new(exact).expect("valid");
        assert!(rule.matches(
            "any",
            &diff("step[2].rep[1].header.x-rift-imposter", "(absent)", "true")
        ));
        assert!(!rule.matches(
            "any",
            &diff("step[2].header.x-rift-imposter", "(absent)", "yes")
        ));
    }

    #[test]
    fn a_wildcard_case_must_pin_both_sides() {
        assert!(Rule::new(entry("*", "step[0].body")).is_err());
        let mut one_side = entry("*", "step[0].body");
        one_side.rift = Some("x".to_string());
        assert!(Rule::new(one_side).is_err());
        let mut json_text = entry("*", "*");
        json_text.class = DiffClass::JsonText;
        assert!(Rule::new(json_text).is_ok());
    }

    #[test]
    fn a_documented_entry_needs_a_citation_that_still_resolves() {
        let mut e = entry("c", "step[0].body");
        e.kind = Kind::Documented;
        assert!(Rule::new(e.clone()).is_err());
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        e.doc = Some("README.md".to_string());
        e.quote = Some("Mountebank   is the\n oracle".to_string());
        let rule = Rule::new(e.clone()).expect("valid");
        assert!(check_citations(std::slice::from_ref(&rule), root).is_ok());
        e.quote = Some("a sentence no doc contains".to_string());
        let rule = Rule::new(e).expect("valid");
        assert!(check_citations(std::slice::from_ref(&rule), root).is_err());
    }
}
