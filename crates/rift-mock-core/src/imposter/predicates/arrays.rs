//! Mountebank's rule for a request field whose actual value is an array (`predicates.js`
//! `predicateSatisfied` and `deepEquals`, 2.9.5 `:238-279`, `:310-323`).
//!
//! One home for the rule, so every array-valued surface answers the same way: a repeated query or
//! form key and a repeated header (issue #1553), the values a jsonpath/xpath selector picks
//! (#1257), and the arrays inside a JSON body (#1579). Each caller decides what counts as an array
//! and hands over a `test` that compares one expected value with one actual value the way that
//! field compares scalars (`except`, case folding and the operator are the caller's).
//!
//! A field that holds a single value is not an array: the caller compares it directly. That is
//! where a one-element array expectation misses a key sent once (`{"key": ["first"]}` against
//! `?key=first`), as in Mountebank.

use serde_json::Value;

/// `predicateSatisfied` for one field whose actual value is the array `actuals`:
///
/// - an expected array holds when **every** expected element is satisfied by **some** actual
///   value, in any order, with extra actual values allowed (`allExpectedArrayValuesMatchActualArray`);
/// - any other expectation holds when **some** actual value satisfies it
///   (`expectedMatchesAtLeastOneValueInActualArray`).
pub(crate) fn satisfied_by_array<T>(
    expected: &Value,
    actuals: &[T],
    test: impl Fn(&Value, &T) -> bool,
) -> bool {
    match expected {
        Value::Array(elements) => elements
            .iter()
            .all(|element| actuals.iter().any(|actual| test(element, actual))),
        _ => actuals.iter().any(|actual| test(expected, actual)),
    }
}

/// `deepEquals` for one field whose actual value is the array `actuals`: Mountebank sorts both
/// arrays and compares them whole, so the expectation must be an array holding the same values as
/// a multiset — same length, each actual value used once, duplicates counted.
///
/// Matched greedily: `test` is an equality (after `except` and case folding), so whichever equal
/// actual value an expected element takes, the remaining ones can still pair up.
pub(crate) fn deep_equals_array<T>(
    expected: &Value,
    actuals: &[T],
    test: impl Fn(&Value, &T) -> bool,
) -> bool {
    let Value::Array(elements) = expected else {
        return false;
    };
    if elements.len() != actuals.len() {
        return false;
    }
    let mut unused: Vec<&T> = actuals.iter().collect();
    elements.iter().all(
        |element| match unused.iter().position(|actual| test(element, actual)) {
            Some(i) => {
                unused.swap_remove(i);
                true
            }
            None => false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn eq(expected: &Value, actual: &&str) -> bool {
        expected.as_str() == Some(*actual)
    }

    #[test]
    fn an_expected_array_needs_every_element_among_the_actuals_in_any_order() {
        assert!(satisfied_by_array(&json!(["b", "a"]), &["a", "b"], eq));
        assert!(satisfied_by_array(&json!(["1", "2"]), &["2", "1", "3"], eq));
        assert!(!satisfied_by_array(&json!(["a", "c"]), &["a", "b"], eq));
        // An expected duplicate is satisfied by the one actual value it repeats.
        assert!(satisfied_by_array(&json!(["a", "a"]), &["a", "b"], eq));
        assert!(satisfied_by_array(&json!([]), &["a", "b"], eq));
    }

    #[test]
    fn a_scalar_expectation_needs_some_actual() {
        assert!(satisfied_by_array(&json!("b"), &["a", "b"], eq));
        assert!(!satisfied_by_array(&json!("c"), &["a", "b"], eq));
    }

    #[test]
    fn deep_equals_is_multiset_equality() {
        assert!(deep_equals_array(&json!(["b", "a"]), &["a", "b"], eq));
        assert!(deep_equals_array(&json!(["a", "a"]), &["a", "a"], eq));
        assert!(!deep_equals_array(&json!(["a", "a"]), &["a", "b"], eq));
        assert!(!deep_equals_array(&json!(["a", "b"]), &["a", "a"], eq));
        assert!(!deep_equals_array(&json!(["a"]), &["a", "a"], eq));
        assert!(!deep_equals_array(&json!(["a", "b", "c"]), &["a", "b"], eq));
        assert!(!deep_equals_array(&json!("a"), &["a", "b"], eq));
    }
}
