//! Wait behavior - add latency before response.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

// Fixed patterns for the no-`javascript`-feature wait fallback (issue #481): compile once at
// first use instead of on every request. These are compile-time-constant patterns, so a compile
// failure is a programming error caught immediately by tests, not a data-dependent runtime error.
static WAIT_FLOOR_OFFSET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Math\.floor\s*\(\s*Math\.random\s*\(\s*\)\s*\*\s*(\d+)\s*\)\s*\+\s*(\d+)")
        .expect("wait floor+offset pattern is a valid constant regex")
});
static WAIT_RANDOM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Math\.random\s*\(\s*\)\s*\*\s*(\d+)")
        .expect("wait random pattern is a valid constant regex")
});
static WAIT_SOLO_MIN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"var\s+min\s*=\s*Math\.ceil\s*\(\s*(\d+)\s*\)")
        .expect("wait solo-min pattern is a valid constant regex")
});
static WAIT_SOLO_MAX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"var\s+max\s*=\s*Math\.floor\s*\(\s*(\d+)\s*\)")
        .expect("wait solo-max pattern is a valid constant regex")
});

/// Wait behavior - add latency before response
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum WaitBehavior {
    /// Fixed delay in milliseconds
    Fixed(u64),
    /// Random delay within range
    Range {
        #[serde(rename = "min")]
        min_ms: u64,
        #[serde(rename = "max")]
        max_ms: u64,
    },
    /// JavaScript function that returns delay — the Mountebank-compatible spelling.
    Function(String),
    /// The same JavaScript function in Rift's object spelling (issue #608), as written by
    /// `docs/features/fault-injection.md`, the shipped examples, and the SDKs. Executes
    /// identically to [`Self::Function`]; kept as a distinct variant only so a config
    /// round-trips to the spelling its author wrote. Not portable to Mountebank, which has no
    /// object form — like [`Self::Range`], this is a documented Rift superset.
    Inject { inject: String },
}

impl WaitBehavior {
    /// Get the wait duration in milliseconds
    pub fn get_duration_ms(&self) -> u64 {
        match self {
            WaitBehavior::Fixed(ms) => *ms,
            WaitBehavior::Range { min_ms, max_ms } => {
                use rand::RngExt;
                // `random_range` asserts a non-empty range, and this runs on the request task with no
                // `catch_unwind` — an inverted range killed the worker on every request (#1148).
                // The parse door refuses that shape, but this is a `pub` enum with `pub` fields, so
                // the draw has to be total on its own. Clamp to `min` like the two sibling draw
                // sites; swapping would quietly honour a config the door rejects.
                if max_ms <= min_ms {
                    if max_ms < min_ms {
                        tracing::warn!(
                            "wait range has min {min_ms} greater than max {max_ms}; clamping to \
                             min. The parse door refuses this, so reaching it means the value was \
                             built in-process rather than loaded"
                        );
                    }
                    *min_ms
                } else {
                    rand::rng().random_range(*min_ms..=*max_ms)
                }
            }
            // Both spellings of a JS-function wait run the identical path (issue #608): same Boa
            // execution, same loud fallback.
            WaitBehavior::Function(js_func) | WaitBehavior::Inject { inject: js_func } => {
                // Parse JavaScript function and execute
                // Format: "function() { return Math.floor(Math.random() * 100) + 50; }"
                // A failed/unusable wait function falls back to 100ms — but loudly, so it is
                // distinguishable from a genuine 100ms wait (B4, issue #355).
                // No cap (issue #1612): like a numeric `wait` and like Mountebank, the computed
                // delay is honoured as written. The sleep is an async `tokio::time::sleep`, which
                // saturates an overflowing deadline, so a huge value holds no worker thread.
                Self::execute_js_wait_function(js_func).unwrap_or_else(|| {
                    tracing::warn!(
                        target: "rift::script",
                        "wait function produced no usable delay; falling back to 100ms"
                    );
                    100
                })
            }
        }
    }

    /// Compute the delay for a string `wait` the way Mountebank does (`behaviors.js` `wait`,
    /// issue #1588): `parseInt` first, so `"1000"` is 1000 and `"800ms"` is 800; otherwise the
    /// source is called as a function, `(<source>)()`, so any callable expression works — a
    /// `function`, an arrow, `async`, a parenthesised function. There is deliberately no prefix
    /// check: Mountebank has none, and a copied routing rule is what kept arrows at 100 ms.
    ///
    /// When the `javascript` feature is enabled the call runs on a bounded Boa `Context` (issue
    /// #355 Item 6). Without it, a regex fallback reads the common shapes, so
    /// `--no-default-features` still builds and works for them. `None` means the source threw or
    /// could not be evaluated, and the caller takes its logged 100 ms fallback.
    fn execute_js_wait_function(js_func: &str) -> Option<u64> {
        let parsed = rift_types::status::js_parse_int(js_func);
        if !parsed.is_nan() {
            return Some(node_timeout_ms(parsed));
        }
        let trimmed = js_func.trim();

        #[cfg(feature = "javascript")]
        {
            if let Some(ms) = Self::execute_js_wait_function_boa(trimmed) {
                return Some(ms);
            }
        }

        Self::execute_js_wait_function_regex(trimmed)
    }

    /// Run the wait function for real on a bounded Boa `Context`: evaluate `(<js_func>)()` and
    /// read the result as Node's `setTimeout` reads a delay ([`node_timeout_ms`]). `None` only for
    /// a script that throws or does not parse, including a throw while converting the result.
    #[cfg(feature = "javascript")]
    fn execute_js_wait_function_boa(js_func: &str) -> Option<u64> {
        let mut context = crate::scripting::bounded_js_context();
        let wrapped = format!("({js_func})()");
        // A script/runtime error must not be swallowed silently (B4, issue #355): log it, then
        // return None so the caller's regex safety net / 100ms fallback still applies.
        let result = match context.eval(boa_engine::Source::from_bytes(wrapped.as_bytes())) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "rift::script", "wait function script error: {e}");
                return None;
            }
        };
        // `to_number` fails only when the conversion itself throws (a `valueOf` that throws, a
        // Symbol) — a throw, like the one above. A string, a Promise or `undefined` converts to
        // `NaN`, which is a value, not an error.
        let n = match result.to_number(&mut context) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    target: "rift::script",
                    "wait function result could not be converted to a number: {e}"
                );
                return None;
            }
        };
        Some(node_timeout_ms(n))
    }

    /// Regex-based fallback: used as the sole path when the `javascript` feature is disabled,
    /// and as a safety net if the Boa path above fails to produce a value while it's enabled.
    fn execute_js_wait_function_regex(trimmed: &str) -> Option<u64> {
        if let Some(body) = extract_function_body(trimmed) {
            // Handle Solo pattern:
            // var min = Math.ceil(N); var max = Math.floor(M); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait;
            if body.contains("var min") && body.contains("var max") {
                return Self::parse_solo_wait_pattern(&body);
            }

            // Look for patterns like "Math.floor(Math.random() * 100) + 50"
            // or "return Math.floor(Math.random() * 100) + 50;"
            let body = body
                .replace("return ", "")
                .trim_end_matches(';')
                .to_string();

            // Parse: Math.floor(Math.random() * N) + M
            if body.contains("Math.random()") {
                use rand::RngExt;
                // Extract multiplier and offset using the cached constant patterns.
                if let Some(caps) = WAIT_FLOOR_OFFSET_RE.captures(&body) {
                    let range = caps.get(1)?.as_str().parse::<u64>().ok()?;
                    let offset = caps.get(2)?.as_str().parse::<u64>().ok()?;
                    return Some(rand::rng().random_range(offset..=offset.saturating_add(range)));
                }

                // Simpler pattern: Math.random() * N
                if let Some(caps) = WAIT_RANDOM_RE.captures(&body) {
                    let range = caps.get(1)?.as_str().parse::<u64>().ok()?;
                    return Some(rand::rng().random_range(0..=range));
                }
            }

            // Try to parse as a simple number. Parse signed so a negative literal clamps to 0
            // (matching the Boa path) instead of failing to parse and dropping to the 100ms
            // fallback (issue #490).
            body.trim().parse::<i64>().ok().map(|v| v.max(0) as u64)
        } else {
            // An arrow with a bare numeric body, `() => 250` (issue #1588). Only a literal: this
            // fallback is not a JavaScript parser.
            let (_, body) = trimmed.split_once("=>")?;
            body.trim()
                .trim_end_matches(';')
                .trim_end()
                .parse::<i64>()
                .ok()
                .map(|v| v.max(0) as u64)
        }
    }

    /// Parse Solo wait pattern:
    /// var min = Math.ceil(N); var max = Math.floor(M); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait;
    fn parse_solo_wait_pattern(body: &str) -> Option<u64> {
        use rand::RngExt;

        // Extract min value: var min = Math.ceil(N)
        let min_val = WAIT_SOLO_MIN_RE
            .captures(body)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);

        // Extract max value: var max = Math.floor(N)
        let max_val = WAIT_SOLO_MAX_RE
            .captures(body)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);

        // Generate random value in range [min, max]
        if max_val >= min_val {
            Some(rand::rng().random_range(min_val..=max_val))
        } else {
            Some(min_val)
        }
    }
}

/// A computed delay as Node's `setTimeout` reads it (issue #1588): `NaN`, an infinity or a
/// negative number is no delay; anything else is floored, and a value beyond `u64` saturates to
/// `u64::MAX`, which `tokio::time::sleep` saturates to its far-future deadline (issue #1612).
fn node_timeout_ms(n: f64) -> u64 {
    if !n.is_finite() || n <= 0.0 {
        return 0;
    }
    // Finite and positive: `as` floors and saturates at `u64::MAX`, which is the intent.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = n.floor() as u64;
    ms
}

/// Extract function body from JavaScript function string: the text between the first `{` and the
/// last `}`, or `None` when either is missing or the `}` comes first.
fn extract_function_body(js_func: &str) -> Option<String> {
    let start = js_func.find('{')?;
    let end = js_func.rfind('}')?;
    js_func
        .get(start + 1..end)
        .map(|body| body.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC 608-1: every documented wait spelling deserializes, and each round-trips to the JSON the
    // author wrote — `GET /imposters?replayable=true` must not rewrite one spelling into another.
    #[test]
    fn wait_accepts_all_four_shapes_and_round_trips() {
        for raw in [
            r#"100"#,
            r#"{"min":100,"max":200}"#,
            r#""function() { return 42; }""#,
            r#"{"inject":"function() { return 42; }"}"#,
        ] {
            let wait: WaitBehavior =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("{raw} must parse: {e}"));
            let back = serde_json::to_string(&wait).expect("serialize");
            let (a, b): (serde_json::Value, serde_json::Value) = (
                serde_json::from_str(raw).expect("raw json"),
                serde_json::from_str(&back).expect("round-trip json"),
            );
            assert_eq!(a, b, "{raw} must round-trip unchanged, got {back}");
        }
    }

    // AC 608-1: the object shapes are disjoint, so untagged resolution is unambiguous.
    #[test]
    fn wait_object_shapes_resolve_to_distinct_variants() {
        let inject: WaitBehavior =
            serde_json::from_str(r#"{"inject":"function() { return 1; }"}"#).expect("inject");
        assert!(matches!(inject, WaitBehavior::Inject { .. }));

        let range: WaitBehavior = serde_json::from_str(r#"{"min":1,"max":2}"#).expect("range");
        assert!(matches!(range, WaitBehavior::Range { .. }));

        // A wait object that is neither shape is still rejected — the new variant must not turn
        // the enum into a catch-all that silently accepts nonsense.
        assert!(serde_json::from_str::<WaitBehavior>(r#"{"bogus":true}"#).is_err());
        assert!(serde_json::from_str::<WaitBehavior>(r#"{"inject":42}"#).is_err());
    }

    // AC 608-2: the object form is a spelling of the same JS-function wait — identical execution,
    // not a second implementation. Holds on both the Boa path and the regex fallback, since the
    // variants share `execute_js_wait_function`.
    #[test]
    fn wait_inject_executes_identically_to_bare_string() {
        let js = "function() { return Math.floor(Math.random() * 0) + 250; }";
        let bare = WaitBehavior::Function(js.to_string());
        let object = WaitBehavior::Inject {
            inject: js.to_string(),
        };
        assert_eq!(bare.get_duration_ms(), object.get_duration_ms());
        assert_eq!(object.get_duration_ms(), 250);
    }

    // AC 608-2: the object form gives the same delay as the bare string for a large value (no cap,
    // issue #1612) and shares the loud fallback (issue #355).
    #[test]
    fn wait_inject_shares_value_and_fallback() {
        let js = "function() { return 999999999; }";
        let huge = WaitBehavior::Inject {
            inject: js.to_string(),
        };
        assert_eq!(
            huge.get_duration_ms(),
            WaitBehavior::Function(js.to_string()).get_duration_ms(),
            "the object form must give the same delay as the bare string form"
        );
        assert_eq!(huge.get_duration_ms(), 999_999_999);

        let unusable = WaitBehavior::Inject {
            inject: "not a function at all".to_string(),
        };
        assert_eq!(
            unusable.get_duration_ms(),
            100,
            "an unusable object-form wait falls back to the same loud 100ms as the bare form"
        );
    }

    #[test]
    fn test_wait_behavior_fixed() {
        let wait = WaitBehavior::Fixed(100);
        assert_eq!(wait.get_duration_ms(), 100);
    }

    #[test]
    fn test_wait_behavior_range() {
        let wait = WaitBehavior::Range {
            min_ms: 100,
            max_ms: 200,
        };
        for _ in 0..10 {
            let duration = wait.get_duration_ms();
            assert!((100..=200).contains(&duration));
        }
    }

    // Issue #1148: `random_range` panics on an empty range, and this ran on the request task with no
    // `catch_unwind` — so an inverted range killed the worker and dropped the connection on every
    // request to that stub, permanently, while the server stayed healthy. The door refuses this
    // shape now, but `WaitBehavior` is a `pub` enum with `pub` fields, so the draw must be total on
    // its own. Clamp to `min` (what the two sibling draw sites already do), never swap: swapping
    // would quietly honour a config the door rejects.
    #[test]
    fn an_inverted_range_clamps_to_min_instead_of_panicking() {
        let wait = WaitBehavior::Range {
            min_ms: 100,
            max_ms: 10,
        };
        // Looped, because a single assertion does not distinguish clamping from *swapping*: a swap
        // would draw from 10..=100 and land on 100 about 1 run in 91. The clamp is deterministic,
        // so every iteration must be 100.
        for _ in 0..200 {
            assert_eq!(wait.get_duration_ms(), 100);
        }
    }

    #[test]
    fn an_equal_bound_range_is_that_value() {
        let wait = WaitBehavior::Range {
            min_ms: 250,
            max_ms: 250,
        };
        assert_eq!(wait.get_duration_ms(), 250);
    }

    // Adjacent unchecked `u64` add on two `\d+` captures. Driven at the regex fallback directly:
    // with the `javascript` feature on, Boa answers first and never reaches it, so going through
    // `get_duration_ms` would pass against the unfixed code and prove nothing. The fallback is the
    // *sole* path in a build without that feature — which, per #1156, is every released artifact.
    #[test]
    fn the_regex_fallback_does_not_overflow_on_a_huge_multiplier() {
        let js = "function() { return Math.floor(Math.random() * 18446744073709551615) + 5; }";
        // The draw is random, so the discriminating assertion is that there *is* a value at all:
        // before the fix `offset + range` overflowed, which panics in debug and wraps to an empty
        // range — then panics inside `random_range` — in release.
        let ms = WaitBehavior::execute_js_wait_function_regex(js)
            .expect("the fallback recognises the pattern");
        assert!(ms >= 5, "the draw starts at the offset, got {ms}");
    }

    #[test]
    fn test_wait_behavior_serde() {
        let yaml = "100";
        let wait: WaitBehavior = serde_norway::from_str(yaml).unwrap();
        assert!(matches!(wait, WaitBehavior::Fixed(100)));

        let yaml = "min: 100\nmax: 200";
        let wait: WaitBehavior = serde_norway::from_str(yaml).unwrap();
        assert!(matches!(
            wait,
            WaitBehavior::Range {
                min_ms: 100,
                max_ms: 200
            }
        ));
    }

    #[test]
    fn test_wait_behavior_solo_js_pattern() {
        // Solo pattern with min=0, max=0 -> returns 0
        let js = " function() { var min = Math.ceil(0); var max = Math.floor(0); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait; } ";
        let wait = WaitBehavior::Function(js.to_string());
        assert_eq!(wait.get_duration_ms(), 0);

        // Solo pattern with min=50, max=100 -> returns value in range
        let js = "function() { var min = Math.ceil(50); var max = Math.floor(100); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait; }";
        let wait = WaitBehavior::Function(js.to_string());
        for _ in 0..10 {
            let duration = wait.get_duration_ms();
            assert!(
                (50..=100).contains(&duration),
                "Duration {duration} not in range 50-100"
            );
        }
    }

    #[test]
    fn test_wait_behavior_js_function() {
        // Simple random pattern
        let js = "function() { return Math.floor(Math.random() * 100) + 50; }";
        let wait = WaitBehavior::Function(js.to_string());
        for _ in 0..10 {
            let duration = wait.get_duration_ms();
            assert!(
                (50..=150).contains(&duration),
                "Duration {duration} not in range 50-150"
            );
        }
    }

    // Issue #355 Item 6: the wait function body is actually EXECUTED (not regex-scraped), so a
    // function that isn't one of the previously-recognized `Math.random` shapes still works.
    #[test]
    fn wait_function_executes_js() {
        let wait = WaitBehavior::Function("function() { return 42; }".to_string());
        assert_eq!(wait.get_duration_ms(), 42);
    }

    // A negative return value must not become an enormous u64 delay (underflow) or a negative
    // sleep; it clamps to 0.
    #[test]
    fn wait_function_negative_clamps_to_zero() {
        let wait = WaitBehavior::Function("function() { return -5; }".to_string());
        assert_eq!(wait.get_duration_ms(), 0);
    }

    // Issue #1612: a function wait is honoured as written, like a numeric `wait` and like
    // Mountebank (`setTimeout(resolve, milliseconds)`, no cap). It used to clamp to 60 s.
    #[test]
    fn wait_function_huge_value_is_honoured() {
        let wait = WaitBehavior::Function("function() { return 999999999; }".to_string());
        assert_eq!(wait.get_duration_ms(), 999_999_999);
    }

    // Issue #1612: a result beyond u64 saturates to u64::MAX rather than panicking or wrapping;
    // `tokio::time::sleep` saturates that to its far-future deadline, the same as a numeric
    // `wait` of u64::MAX. Boa only: the regex fallback does not read exponent literals.
    #[cfg(feature = "javascript")]
    #[test]
    fn wait_function_beyond_u64_saturates() {
        let wait = WaitBehavior::Function("function() { return 1e300; }".to_string());
        assert_eq!(wait.get_duration_ms(), u64::MAX);
    }

    // Issue #481: the no-`javascript`-feature regex fallback now uses shared LazyLock statics.
    // Exercise it directly (the default-feature tests above hit the Boa path instead) so every
    // constant pattern is compiled and matched — this pins that the statics' `.expect` never
    // fires and the patterns still recognize each shape.
    #[test]
    fn wait_regex_fallback_parses_all_patterns() {
        // Math.floor(Math.random() * N) + M
        let floor = WaitBehavior::execute_js_wait_function_regex(
            "function() { return Math.floor(Math.random() * 100) + 50; }",
        );
        assert!(matches!(floor, Some(d) if (50..=150).contains(&d)));

        // Math.random() * N (no offset)
        let random = WaitBehavior::execute_js_wait_function_regex(
            "function() { return Math.floor(Math.random() * 30); }",
        );
        assert!(matches!(random, Some(d) if d <= 30));

        // Solo pattern (var min = Math.ceil(N); var max = Math.floor(M); ...)
        let solo = WaitBehavior::execute_js_wait_function_regex(
            "function() { var min = Math.ceil(50); var max = Math.floor(100); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait; }",
        );
        assert!(matches!(solo, Some(d) if (50..=100).contains(&d)));

        // Issue #490: a negative literal clamps to 0 in the regex fallback (was: parse::<u64> ->
        // None -> 100ms), matching the Boa path.
        assert_eq!(
            WaitBehavior::execute_js_wait_function_regex("function() { return -5; }"),
            Some(0)
        );
    }

    // Issue #1384: a body the regex fallback cannot parse (it would answer the 100ms fallback), so
    // only a real Boa evaluation yields 42.
    #[cfg(feature = "javascript")]
    #[test]
    fn a_wait_function_only_boa_can_evaluate_runs_on_boa() {
        let wait = WaitBehavior::Function("function() { return 2 * 21; }".to_string());
        assert_eq!(wait.get_duration_ms(), 42);
    }

    // Issue #1384, revised by #1588: a non-finite result does not saturate to `u64::MAX`
    // (`Infinity`). Node's `setTimeout` reads it as no delay, so Rift does too — it used to take
    // the 100 ms fallback. Boa only: the regex fallback does not read these names.
    #[cfg(feature = "javascript")]
    #[test]
    fn a_non_finite_wait_is_no_delay() {
        for body in ["Infinity", "NaN"] {
            let wait = WaitBehavior::Function(format!("function() {{ return {body}; }}"));
            assert_eq!(wait.get_duration_ms(), 0, "return {body}");
        }
    }

    // Issue #1588: Mountebank's `wait` reads a string with `parseInt` first and only otherwise
    // evaluates `(<source>)()`, so any callable expression is a wait function — not just one that
    // starts with `function`. Measured as the computed delay, never as wall time.
    #[cfg(feature = "javascript")]
    #[test]
    fn any_callable_wait_expression_runs_like_mountebank() {
        for (source, ms) in [
            ("() => 1000", 1000),
            ("() => { return 700; }", 700),
            ("x => 300", 300),
            ("(function(){return 400})", 400),
            ("async function () { return 5; }", 0),
            ("function () { return '500'; }", 500),
        ] {
            let wait = WaitBehavior::Function(source.to_string());
            assert_eq!(wait.get_duration_ms(), ms, "wait {source:?}");
        }
    }

    // Issue #1588: Node's `setTimeout` treats a non-number, `NaN`, an infinity or a negative delay
    // as no delay, so these answer at once in Mountebank instead of taking the 100 ms fallback.
    #[cfg(feature = "javascript")]
    #[test]
    fn a_non_numeric_wait_result_is_no_delay() {
        for source in [
            "async () => 1",
            "() => 'abc'",
            "() => -5",
            "function() { return Infinity; }",
            "function() { return -Infinity; }",
            "function() { return NaN; }",
            "() => undefined",
        ] {
            let wait = WaitBehavior::Function(source.to_string());
            assert_eq!(wait.get_duration_ms(), 0, "wait {source:?}");
        }
    }

    // Issue #1588: a throwing wait function still takes the 100 ms fallback, and is not mistaken
    // for a non-numeric result (no delay). Mountebank answers 500 here; that is #1616.
    #[test]
    fn a_throwing_wait_function_falls_back_to_100ms() {
        let wait = WaitBehavior::Function("() => { throw new Error('x'); }".to_string());
        assert_eq!(wait.get_duration_ms(), 100);
    }

    // Issue #1588: `parseInt` runs first, so a numeric string is its number, a trailing unit is
    // ignored, and none of it needs a JavaScript engine — this holds in a no-`javascript` build.
    #[test]
    fn a_numeric_wait_string_is_read_with_parse_int() {
        for (source, ms) in [
            ("1000", 1000),
            ("800ms", 800),
            ("  250  ", 250),
            ("0", 0),
            ("-5", 0),
            ("0x10", 16),
            ("1e3", 1),
        ] {
            let wait = WaitBehavior::Function(source.to_string());
            assert_eq!(wait.get_duration_ms(), ms, "wait {source:?}");
            let inject = WaitBehavior::Inject {
                inject: source.to_string(),
            };
            assert_eq!(inject.get_duration_ms(), ms, "inject {source:?}");
        }
    }

    // Issue #1588: without Boa, the regex fallback also reads an arrow with a bare numeric body.
    #[test]
    fn the_regex_fallback_reads_a_bare_arrow_body() {
        assert_eq!(
            WaitBehavior::execute_js_wait_function_regex("() => 250"),
            Some(250)
        );
        assert_eq!(
            WaitBehavior::execute_js_wait_function_regex("x => 300;"),
            Some(300)
        );
        assert_eq!(
            WaitBehavior::execute_js_wait_function_regex("() => { return 700; }"),
            Some(700)
        );
    }

    // Issue #1384: the solo pattern needs both `var min` and `var max`; `min` alone is not it.
    #[test]
    fn regex_fallback_needs_both_min_and_max_for_the_solo_pattern() {
        assert_eq!(
            WaitBehavior::execute_js_wait_function_regex(
                "function() { var min = Math.ceil(5); return 7; }"
            ),
            None
        );
    }

    // Issue #1384: an inverted solo pattern answers `min` instead of drawing from an empty range,
    // which `random_range` panics on.
    #[test]
    fn an_inverted_solo_pattern_returns_min() {
        assert_eq!(
            WaitBehavior::parse_solo_wait_pattern(
                "var min = Math.ceil(20); var max = Math.floor(10);"
            ),
            Some(20)
        );
    }

    // Issue #1384: the clamp of an inverted range is the documented loud fallback, so it warns.
    #[tracing_test::traced_test]
    #[test]
    fn an_inverted_range_warns() {
        let wait = WaitBehavior::Range {
            min_ms: 5,
            max_ms: 3,
        };
        assert_eq!(wait.get_duration_ms(), 5);
        assert!(logs_contain("clamping to min"));
    }

    // Issue #1384: an equal-bound range is a valid config, so it answers its value silently.
    #[tracing_test::traced_test]
    #[test]
    fn an_equal_range_does_not_warn() {
        let wait = WaitBehavior::Range {
            min_ms: 5,
            max_ms: 5,
        };
        assert_eq!(wait.get_duration_ms(), 5);
        assert!(!logs_contain("clamping to min"));
    }

    // Issue #1384: a `}` before the first `{` leaves no body to extract.
    #[test]
    fn a_closing_brace_before_the_opening_one_has_no_body() {
        assert_eq!(extract_function_body("function() } 5 {"), None);
        assert_eq!(extract_function_body("function() {}").as_deref(), Some(""));
    }
}
