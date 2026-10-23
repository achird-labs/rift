//! How Mountebank reads a `statusCode` string when it writes a response (issue #1558).
//!
//! Mountebank stores `statusCode` as written and hands it to Node's `response.writeHead`, which
//! computes `statusCode |= 0` — JavaScript's `ToInt32(Number(value))` — and throws
//! `ERR_HTTP_INVALID_STATUS_CODE` for a result outside 100–999. The engine resolves a string status
//! with these functions, and `rift-lint` judges one with them, so the two cannot disagree.
//!
//! [`js_parse_int`] lives here for the same reason: Mountebank reads a `wait` string with
//! `parseInt` (issue #1588), and the engine and `rift-lint` both need that one reading.

/// The status Node serves for `text`, or `None` when Node refuses it (outside 100–999).
///
/// The empty string is not special-cased here: Mountebank treats it as falsy and falls back to the
/// `defaultResponse` status before Node sees it, which is the caller's to decide.
#[must_use]
pub fn node_status_code(text: &str) -> Option<u16> {
    u16::try_from(js_to_int32(js_string_to_number(text)))
        .ok()
        .filter(|code| (100..=999).contains(code))
}

/// JavaScript's `Number(text)` for a string (ECMA-262 `StringToNumber`): surrounding whitespace is
/// ignored, an empty string is `0`, `0x`/`0o`/`0b` integers and `Infinity` are read, and anything
/// else that is not a decimal literal is `NaN`.
#[must_use]
pub fn js_string_to_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(is_js_whitespace);
    if trimmed.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        let head = trimmed.get(..2).map(str::to_ascii_lowercase);
        if head.as_deref() == Some(prefix) {
            return radix_integer(&trimmed[2..], radix);
        }
    }
    let (sign, unsigned) = match trimmed.as_bytes()[0] {
        b'+' => (1.0, &trimmed[1..]),
        b'-' => (-1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    if unsigned == "Infinity" {
        return sign * f64::INFINITY;
    }
    if !is_unsigned_decimal_literal(unsigned) {
        return f64::NAN;
    }
    // The grammar check above leaves only what Rust's float parser reads the same way.
    unsigned.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// JavaScript's `parseInt(text)` with no radix: leading whitespace is skipped, then an optional
/// sign, then a `0x`/`0X` prefix selects hexadecimal; the result is the longest run of digits that
/// follows, and anything after it is ignored (`"800ms"` is `800`, `"1e3"` is `1`). No digits at all
/// is `NaN`.
#[must_use]
pub fn js_parse_int(text: &str) -> f64 {
    let rest = text.trim_start_matches(is_js_whitespace);
    let (sign, rest) = match rest.as_bytes().first() {
        Some(b'+') => (1.0, &rest[1..]),
        Some(b'-') => (-1.0, &rest[1..]),
        _ => (1.0, rest),
    };
    let (radix, digits) = match rest.get(..2) {
        Some("0x" | "0X") => (16, &rest[2..]),
        _ => (10, rest),
    };
    let run = digits
        .find(|c: char| !c.is_digit(radix))
        .map_or(digits, |end| &digits[..end]);
    sign * radix_integer(run, radix)
}

/// JavaScript's `ToInt32`: `NaN` and the infinities are `0`; anything else is truncated and wrapped
/// modulo 2³² into the signed range.
#[must_use]
pub fn js_to_int32(number: f64) -> i32 {
    if !number.is_finite() {
        return 0;
    }
    const TWO_32: f64 = 4_294_967_296.0;
    let wrapped = number.trunc().rem_euclid(TWO_32);
    let signed = if wrapped >= TWO_32 / 2.0 {
        wrapped - TWO_32
    } else {
        wrapped
    };
    // In range by construction: `signed` is an integer within [-2^31, 2^31).
    #[allow(clippy::cast_possible_truncation)]
    let int = signed as i32;
    int
}

/// ECMA-262 `WhiteSpace` and `LineTerminator`: Unicode `White_Space` without U+0085, plus U+FEFF.
fn is_js_whitespace(c: char) -> bool {
    c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}')
}

/// A non-empty run of digits in `radix`, with no sign; `NaN` otherwise.
fn radix_integer(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() {
        return f64::NAN;
    }
    digits
        .chars()
        .try_fold(0.0, |acc: f64, c| {
            c.to_digit(radix)
                .map(|d| acc.mul_add(f64::from(radix), f64::from(d)))
        })
        .unwrap_or(f64::NAN)
}

/// `StrUnsignedDecimalLiteral` without `Infinity`: `digits[.digits][e[+-]digits]`, or
/// `.digits[e[+-]digits]`, with digits on at least one side of the point.
fn is_unsigned_decimal_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits_from = |at: usize| {
        bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let mut at = digits_from(0);
    let mut mantissa_digits = at;
    if bytes.get(at) == Some(&b'.') {
        let fraction = digits_from(at + 1);
        mantissa_digits += fraction;
        at += 1 + fraction;
    }
    if mantissa_digits == 0 {
        return false;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let exponent = digits_from(at);
        if exponent == 0 {
            return false;
        }
        at += exponent;
    }
    at == bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// `parseInt(s)` as Node 22 prints it, for each input (issue #1588).
    #[test]
    fn parse_int_matches_node_for_a_table_of_inputs() {
        for (text, node) in [
            ("1000", 1000.0),
            ("800ms", 800.0),
            ("  \t\n250  ", 250.0),
            ("\u{feff}7", 7.0),
            ("+12", 12.0),
            ("-5", -5.0),
            ("0x10", 16.0),
            ("0XfF", 255.0),
            ("-0x10", -16.0),
            ("1e3", 1.0),
            ("12.9", 12.0),
            ("007", 7.0),
            ("4294967296", 4_294_967_296.0),
        ] {
            assert_eq!(js_parse_int(text), node, "parseInt({text:?})");
        }
        for text in [
            "",
            "  ",
            "ms",
            "-",
            "+",
            "0x",
            "0xg",
            ".5",
            "() => 1000",
            "function () { return 1; }",
        ] {
            assert!(js_parse_int(text).is_nan(), "parseInt({text:?}) is NaN");
        }
    }

    /// `Number(s) | 0` as Node 22 prints it, for each input.
    #[test]
    fn matches_node_for_a_table_of_inputs() {
        for (text, node) in [
            ("404", 404),
            (" 404 ", 404),
            ("\t404\n", 404),
            ("\u{feff}404\u{a0}", 404),
            ("404.7", 404),
            ("-404.7", -404),
            ("0x194", 404),
            ("0X194", 404),
            ("0o624", 404),
            ("0b110010100", 404),
            ("1e2", 100),
            ("4.04E2", 404),
            (".5e3", 500),
            ("5.", 5),
            ("+404", 404),
            ("-404", -404),
            ("abc", 0),
            ("${code}", 0),
            ("4x0", 0),
            ("", 0),
            ("   ", 0),
            ("Infinity", 0),
            ("-Infinity", 0),
            ("infinity", 0),
            ("inf", 0),
            ("NaN", 0),
            ("-0x194", 0),
            ("0x", 0),
            ("1_000", 0),
            ("e5", 0),
            (".", 0),
            ("1e", 0),
            ("\u{85}404", 0),
            ("4294967700", 404),
            ("2147483648", -2_147_483_648),
            ("1e21", -559_939_584),
        ] {
            assert_eq!(
                js_to_int32(js_string_to_number(text)),
                node,
                "Number({text:?}) | 0"
            );
        }
    }

    #[test]
    fn node_status_code_serves_only_100_to_999() {
        assert_eq!(node_status_code("404"), Some(404));
        assert_eq!(node_status_code(" 999 "), Some(999));
        assert_eq!(node_status_code("100"), Some(100));
        assert_eq!(node_status_code("99"), None);
        assert_eq!(node_status_code("1000"), None);
        assert_eq!(node_status_code("-404"), None);
        assert_eq!(node_status_code("${code}"), None);
        assert_eq!(node_status_code(""), None);
    }

    /// `ToInt32` truncates toward zero *before* it wraps, so it is not periodic in 2³² across zero
    /// for a fraction: `trunc(-0.5)` is a ceiling, `trunc(2³² - 0.5)` a floor. These are Node's
    /// answers (`(-0.5)|0 === 0`, `4294967295.5|0 === -1`); the old periodicity property drew real
    /// numbers and failed on exactly this pair (issue #1656).
    #[test]
    fn to_int32_is_not_periodic_across_zero_for_fractions() {
        assert_eq!(js_to_int32(-0.5), 0);
        assert_eq!(js_to_int32(4_294_967_295.5), -1);
    }

    proptest! {
        /// Any integer spelled in decimal, hex or with padding reads as itself, wrapped to 32 bits.
        #[test]
        fn an_integer_reads_as_itself(n in any::<i64>(), pad in "[ \t\n]{0,3}") {
            let expected = js_to_int32(n as f64);
            prop_assert_eq!(js_to_int32(js_string_to_number(&format!("{pad}{n}{pad}"))), expected);
            if (0..1_i64 << 53).contains(&n) {
                prop_assert_eq!(js_to_int32(js_string_to_number(&format!("0x{n:x}"))), expected);
            }
        }

        /// `ToInt32` is periodic in 2³² on the integers, and lands in the signed 32-bit range.
        #[test]
        fn to_int32_wraps_modulo_two_to_the_32(k in -(1_i64 << 52)..(1_i64 << 52)) {
            let n = k as f64; // exact: |k| < 2^53
            prop_assert_eq!(js_to_int32(n), js_to_int32(n + 4_294_967_296.0)); // exact: |n + 2^32| < 2^53
            prop_assert_eq!(i64::from(js_to_int32(n)), i64::from(k as i32));
        }

        /// A fraction is truncated toward zero before the wrap.
        #[test]
        fn to_int32_truncates_toward_zero(n in -1.0e15f64..1.0e15f64) {
            prop_assert_eq!(js_to_int32(n), js_to_int32(n.trunc()));
            prop_assert_eq!(i64::from(js_to_int32(n)), (n.trunc() as i64) as i32 as i64);
        }

        /// Text with a letter outside a hex literal, `e`, or `Infinity` is never a status.
        #[test]
        fn a_word_is_never_a_status(word in "[g-wyzG-WYZ][a-z]{0,6}") {
            prop_assume!(word != "Infinity");
            prop_assert_eq!(node_status_code(&word), None);
        }
    }
}
