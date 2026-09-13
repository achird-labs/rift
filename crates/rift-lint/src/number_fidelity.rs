//! Number literals `serde_json` cannot write back digit-for-digit (issue #1080).
//!
//! A `serde_json::Value` holds a number as `u64`, `i64` or `f64` — `arbitrary_precision` is off in
//! this workspace, and must stay off (see `duplicate_keys`). So a literal wider than `u64`, or with
//! more digits than `f64` carries, is already a different number once parsed, and only the raw text
//! still has the original. This module reads that text for number tokens and asks, per token,
//! whether `serde_json`'s own parse → serialize round-trip keeps its digits.
//!
//! "Keeps its digits" compares the *value* as a decimal, not the spelling: `0.10` → `0.1` and
//! `1e2` → `100.0` are formatting, which `--fix` has never promised to keep.
//!
//! The answer is only as narrow as `serde_json`'s parse is exact. The workspace enables
//! `float_roundtrip` (issue #1085); without it an ordinary float such as `7e23` parses one double
//! away and would be reported here too.

/// A number literal whose digits would change if the document were re-serialized from its parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossyNumber {
    /// 1-based line of the literal's first byte.
    pub line: usize,
    /// 1-based byte column of the literal's first byte.
    pub column: usize,
    /// The literal as written.
    pub literal: String,
    /// What `serde_json` writes back for it.
    pub written_as: String,
}

/// Every lossy number literal in `text`, in document order.
///
/// `text` must already have parsed as JSON: the lexer trusts that every token is well-formed and
/// only tells numbers apart from everything else. Outside a string, JSON's only tokens that
/// contain a digit or `-` are numbers.
pub(crate) fn find(text: &str) -> Vec<LossyNumber> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut line = 1;
    let mut line_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                line += 1;
                line_start = i + 1;
                i += 1;
            }
            // A JSON string cannot hold a raw newline, so skipping it cannot miss a line.
            b'"' => i = end_of_string(bytes, i + 1),
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                {
                    i += 1;
                }
                let literal = &text[start..i];
                if let Some(written_as) = rewritten(literal) {
                    found.push(LossyNumber {
                        line,
                        column: start - line_start + 1,
                        literal: literal.to_string(),
                        written_as,
                    });
                }
            }
            _ => i += 1,
        }
    }
    found
}

/// The index just past the closing quote of a string whose body starts at `i`.
fn end_of_string(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// What `serde_json` writes back for `literal`, when that is a different number.
fn rewritten(literal: &str) -> Option<String> {
    let written_as = match serde_json::from_str::<serde_json::Value>(literal) {
        Ok(value) => value.to_string(),
        // Unreachable for a token of a document that parsed. If it ever is reached, report it:
        // this answer decides whether a file gets overwritten, so "cannot tell" must mean "no".
        Err(e) => return Some(format!("<unparseable: {e}>")),
    };
    (Decimal::of(literal) != Decimal::of(&written_as)).then_some(written_as)
}

/// A number literal's value as `sign × digits × 10^exponent`, with `digits` free of leading and
/// trailing zeros, so two spellings of one value compare equal. Zero is always positive with no
/// digits.
#[derive(Debug, PartialEq, Eq)]
struct Decimal {
    negative: bool,
    digits: String,
    exponent: i64,
}

impl Decimal {
    fn of(number: &str) -> Self {
        let (negative, unsigned) = match number.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, number),
        };
        let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
            // An exponent too large for `i64` saturates; the value it names is out of any `f64`'s
            // reach, so it can only ever compare unequal to what `serde_json` writes back.
            Some((mantissa, exp)) => (
                mantissa,
                exp.parse::<i64>().unwrap_or(if exp.starts_with('-') {
                    i64::MIN
                } else {
                    i64::MAX
                }),
            ),
            None => (unsigned, 0),
        };
        let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let all = format!("{int}{frac}");
        let significant = all.trim_start_matches('0');
        let digits = significant.trim_end_matches('0');
        if digits.is_empty() {
            return Decimal {
                negative: false,
                digits: String::new(),
                exponent: 0,
            };
        }
        let trailing_zeros = significant.len() - digits.len();
        Decimal {
            negative,
            digits: digits.to_string(),
            exponent: exponent
                .saturating_sub(frac.len() as i64)
                .saturating_add(trailing_zeros as i64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lossy(text: &str) -> Vec<(String, String)> {
        find(text)
            .into_iter()
            .map(|n| (n.literal, n.written_as))
            .collect()
    }

    fn pair(literal: &str, written_as: &str) -> (String, String) {
        (literal.to_string(), written_as.to_string())
    }

    #[test]
    fn an_integer_wider_than_u64_is_lossy() {
        assert_eq!(
            lossy(r#"{"big": 123456789012345678901234567890}"#),
            vec![pair(
                "123456789012345678901234567890",
                "1.2345678901234568e29"
            )]
        );
    }

    #[test]
    fn a_decimal_with_more_digits_than_f64_carries_is_lossy() {
        assert_eq!(
            lossy(r#"{"precise": 0.1000000000000000055511151231257827}"#),
            vec![pair("0.1000000000000000055511151231257827", "0.1")]
        );
    }

    #[test]
    fn one_past_u64_max_is_lossy_but_u64_max_is_not() {
        assert_eq!(lossy("[18446744073709551615]"), vec![]);
        assert_eq!(
            lossy("[18446744073709551616]"),
            vec![pair("18446744073709551616", "1.8446744073709552e19")]
        );
    }

    #[test]
    fn one_below_i64_min_is_lossy_but_i64_min_is_not() {
        assert_eq!(lossy("[-9223372036854775808]"), vec![]);
        assert_eq!(
            lossy("[-9223372036854775809]"),
            vec![pair("-9223372036854775809", "-9.223372036854776e18")]
        );
    }

    /// Formatting changes are not information loss — the Auto-Fix docs already say the rewrite
    /// does not preserve formatting. Refusing over these would refuse ordinary files.
    #[test]
    fn a_number_that_only_changes_spelling_is_not_lossy() {
        assert_eq!(
            lossy("[0.10, 1e2, 1E+2, 1.0, 100, 0, -0, -0.0, 0e5, 2.5e-3, 3000, 1.5]"),
            vec![]
        );
    }

    /// Relies on `serde_json`'s `float_roundtrip` (issue #1085). Without it each of these parses one
    /// double away and is written back with different digits, so this module would refuse ordinary
    /// floats — and `1.23e-30` ⇄ `1.2299999999999999e-30` would have no spelling that survives.
    #[test]
    fn a_float_a_double_holds_exactly_in_shortest_form_is_not_lossy() {
        assert_eq!(
            lossy("[7e23, 1e-23, 1.23e-30, 1.2299999999999999e-30, 0.10018513143495411]"),
            vec![]
        );
    }

    #[test]
    fn a_decimal_a_double_can_only_approximate_is_lossy() {
        assert_eq!(
            lossy("[0.30000000000000001]"),
            vec![pair("0.30000000000000001", "0.3")]
        );
    }

    #[test]
    fn an_underflow_to_zero_is_lossy() {
        assert_eq!(lossy("[1e-400]"), vec![pair("1e-400", "0.0")]);
    }

    #[test]
    fn digits_inside_a_string_are_not_a_number() {
        assert_eq!(lossy(r#"{"id": "123456789012345678901234567890"}"#), vec![]);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        assert_eq!(lossy(r#"{"a\"123456789012345678901234567890": 1}"#), vec![]);
    }

    #[test]
    fn an_escaped_backslash_before_the_closing_quote_ends_the_string() {
        assert_eq!(
            lossy(r#"{"a\\": 123456789012345678901234567890}"#),
            vec![pair(
                "123456789012345678901234567890",
                "1.2345678901234568e29"
            )]
        );
    }

    #[test]
    fn every_lossy_literal_is_reported_in_document_order() {
        assert_eq!(
            lossy(r#"[123456789012345678901234567890, 7, 0.1000000000000000055511151231257827]"#),
            vec![
                pair("123456789012345678901234567890", "1.2345678901234568e29"),
                pair("0.1000000000000000055511151231257827", "0.1"),
            ]
        );
    }

    #[test]
    fn the_position_is_the_literal_first_byte() {
        let text = "{\n  \"port\": 3000,\n  \"big\": 123456789012345678901234567890\n}";
        let found = find(text);
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!((found[0].line, found[0].column), (3, 10));
    }

    #[test]
    fn the_position_on_the_first_line_counts_from_column_one() {
        let found = find(r#"{"big":123456789012345678901234567890}"#);
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!((found[0].line, found[0].column), (1, 8));
    }
}
