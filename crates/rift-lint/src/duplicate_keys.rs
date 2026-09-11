//! Byte-identical duplicate-key detection over the raw document text (issue #1069).
//!
//! Every other rule in this crate validates a `serde_json::Value`, and a `Value` cannot answer this
//! question: `serde_json::Map` is last-wins without the `preserve_order` feature, so by the time a
//! document is parsed `{"X-Id": "a", "X-Id": "b"}` is indistinguishable from `{"X-Id": "b"}`.
//! Enabling `preserve_order` would not help either — `IndexMap::insert` is also last-wins, and
//! `rift_types::wire` has a test that deliberately fails if the feature is ever turned on.
//!
//! So this module reads the text a second time with a visitor that records repeats instead of
//! discarding them. It is the same shape as `rift_types::wire::single_value_headers`'s visitor;
//! it is written out here rather than shared because `rift-lint` does not depend on `rift-types`
//! and should not start to for one visitor.
//!
//! Case-variant duplicates (`X-Id` beside `x-id`) are **not** reported here — those survive into
//! the `Value` and belong to E043, which already sees them with full field context.
//!
//! One more `serde_json` feature matters here: with `arbitrary_precision` a bare JSON number stops
//! reaching `visit_f64` and arrives as a one-key map instead, which this scanner would read as a
//! real object. It is off, and `numbers_of_every_shape_scan_as_scalars` below is what fails first
//! if it is ever turned on.

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::collections::HashSet;
use std::fmt;

/// One object that names the same key twice.
#[derive(Debug)]
pub(crate) struct Duplicate {
    /// Path of the containing object in the dotted/indexed form the rest of the linter uses
    /// (`stubs[0].responses[0].proxy.injectHeaders`). `None` is the document's root object, which
    /// has no path to name.
    pub(crate) location: Option<String>,
    /// The key that was given more than once.
    pub(crate) key: String,
}

/// Every byte-identical repeated key in `text`, one entry per repeated occurrence.
///
/// Fails only when `text` is not valid JSON, with the same `serde_json::Error` (and therefore the
/// same line/column) that parsing it into a `Value` would produce. A duplicate key is data this
/// returns, never an error.
pub(crate) fn find(text: &str) -> Result<Vec<Duplicate>, serde_json::Error> {
    let mut de = serde_json::Deserializer::from_str(text);
    let scan = Scan::deserialize(&mut de)?;
    // Matches `from_str`'s strictness: trailing content after the value is an error there too.
    de.end()?;
    Ok(scan.0)
}

/// Findings from one subtree, with locations *relative to that subtree*. Each parent prefixes its
/// own segment as the stack unwinds, which is what builds the full path without threading one
/// down through the whole walk.
struct Scan(Vec<Duplicate>);

/// Prefix `segment` onto a child's relative location.
///
/// An index segment abuts what precedes it (`stubs[0]`), a key segment is dot-separated
/// (`proxy.injectHeaders`) — which is why this looks at the child rather than just concatenating.
fn nest(segment: &str, inner: Option<String>) -> Option<String> {
    Some(match inner {
        None => segment.to_string(),
        Some(inner) if inner.starts_with('[') => format!("{segment}{inner}"),
        Some(inner) => format!("{segment}.{inner}"),
    })
}

impl<'de> Deserialize<'de> for Scan {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ScanVisitor)
    }
}

struct ScanVisitor;

impl<'de> Visitor<'de> for ScanVisitor {
    type Value = Scan;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Scan, M::Error> {
        let mut seen: HashSet<String> = HashSet::with_capacity(access.size_hint().unwrap_or(0));
        let mut found = Vec::new();
        while let Some(key) = access.next_key::<String>()? {
            let child: Scan = access.next_value()?;
            for dup in child.0 {
                found.push(Duplicate {
                    location: nest(&key, dup.location),
                    key: dup.key,
                });
            }
            // `contains` then `insert`, not `insert(key.clone())`: this runs for every key of
            // every object, and the overwhelming majority of documents have no duplicate at all.
            if seen.contains(key.as_str()) {
                // Relative to this object, so the location is empty here; the parent names it.
                found.push(Duplicate {
                    location: None,
                    key,
                });
            } else {
                seen.insert(key);
            }
        }
        Ok(Scan(found))
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut access: S) -> Result<Scan, S::Error> {
        let mut found = Vec::new();
        let mut index = 0usize;
        while let Some(child) = access.next_element::<Scan>()? {
            // Built inside the guard, not per element: a clean array allocates nothing here.
            if !child.0.is_empty() {
                let segment = format!("[{index}]");
                for dup in child.0 {
                    found.push(Duplicate {
                        location: nest(&segment, dup.location),
                        key: dup.key,
                    });
                }
            }
            index += 1;
        }
        Ok(Scan(found))
    }

    // Scalars hold no keys. Every JSON scalar shape gets an arm because the default `Visitor`
    // implementations reject rather than ignore, which would turn a number into a parse error.
    fn visit_bool<E>(self, _: bool) -> Result<Scan, E> {
        Ok(Scan(Vec::new()))
    }
    fn visit_i64<E>(self, _: i64) -> Result<Scan, E> {
        Ok(Scan(Vec::new()))
    }
    fn visit_i128<E>(self, _: i128) -> Result<Scan, E> {
        Ok(Scan(Vec::new()))
    }
    fn visit_u64<E>(self, _: u64) -> Result<Scan, E> {
        Ok(Scan(Vec::new()))
    }
    fn visit_u128<E>(self, _: u128) -> Result<Scan, E> {
        Ok(Scan(Vec::new()))
    }
    fn visit_f64<E>(self, _: f64) -> Result<Scan, E> {
        Ok(Scan(Vec::new()))
    }
    fn visit_str<E>(self, _: &str) -> Result<Scan, E> {
        Ok(Scan(Vec::new()))
    }
    fn visit_unit<E>(self) -> Result<Scan, E> {
        Ok(Scan(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::find;

    fn found(text: &str) -> Vec<(Option<String>, String)> {
        find(text)
            .expect("valid json")
            .into_iter()
            .map(|d| (d.location, d.key))
            .collect()
    }

    #[test]
    fn a_clean_document_yields_nothing() {
        assert!(found(r#"{"a":1,"b":[1,2,{"c":null}],"d":"x"}"#).is_empty());
    }

    #[test]
    fn a_root_level_repeat_has_no_location() {
        assert_eq!(found(r#"{"a":1,"a":2}"#), vec![(None, "a".to_string())]);
    }

    #[test]
    fn a_nested_repeat_is_named_by_its_path() {
        assert_eq!(
            found(r#"{"stubs":[{"responses":[{"proxy":{"injectHeaders":{"X":"a","X":"b"}}}]}]}"#),
            vec![(
                Some("stubs[0].responses[0].proxy.injectHeaders".to_string()),
                "X".to_string()
            )]
        );
    }

    #[test]
    fn each_repeated_occurrence_is_its_own_finding() {
        assert_eq!(found(r#"{"a":1,"a":2,"a":3}"#).len(), 2);
    }

    #[test]
    fn sibling_objects_do_not_share_a_key_space() {
        assert!(found(r#"[{"a":1},{"a":2}]"#).is_empty());
    }

    #[test]
    fn nested_arrays_chain_their_indices() {
        assert_eq!(
            found(r#"{"a":[[{"b":1,"b":2}]]}"#),
            vec![(Some("a[0][0]".to_string()), "b".to_string())]
        );
    }

    /// Numbers must reach the scalar arms, not `visit_map`. If `serde_json`'s
    /// `arbitrary_precision` feature is ever enabled workspace-wide, a bare number arrives as a
    /// synthetic one-key map and this scanner would start treating it as an object — this is the
    /// test that catches that, deliberately and first.
    #[test]
    fn numbers_of_every_shape_scan_as_scalars() {
        for text in [
            r#"{"a":1}"#,
            r#"{"a":-1}"#,
            r#"{"a":1.5}"#,
            r#"{"a":1e308}"#,
            r#"{"a":18446744073709551615}"#,
            r#"{"a":-9223372036854775808}"#,
            r#"{"a":0.1000000000000000055511151231257827}"#,
        ] {
            assert!(found(text).is_empty(), "{text} must scan as a scalar");
        }
    }

    /// A key that literally contains the path separators produces the same location string as the
    /// nesting it imitates. Pinned rather than fixed: `location` is a human-readable path shared
    /// with every other rule in the crate, not a machine-addressable pointer, and changing that
    /// convention is a far larger change than this rule.
    #[test]
    fn a_key_containing_path_syntax_is_ambiguous_by_convention() {
        assert_eq!(
            found(r#"{"a.b":{"x":1,"x":2}}"#),
            vec![(Some("a.b".to_string()), "x".to_string())]
        );
    }

    #[test]
    fn a_case_variant_pair_is_not_a_byte_identical_duplicate() {
        assert!(found(r#"{"X-Id":"a","x-id":"b"}"#).is_empty());
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(find("{not json}").is_err());
    }

    #[test]
    fn trailing_content_is_rejected_like_from_str() {
        assert!(find(r#"{"a":1} trailing"#).is_err());
    }

    #[test]
    fn a_bare_scalar_document_is_accepted_and_empty() {
        for text in ["1", "true", "null", r#""x""#, "[]", "{}"] {
            assert!(find(text).expect("valid json").is_empty(), "{text}");
        }
    }
}
