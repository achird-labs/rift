//! Wait behavior - add latency before response.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// Hard cap on a computed wait delay (issues #355, #490): a wait function that returns a huge
/// value must not sleep the worker unbounded. Applied at the `get_duration_ms` boundary so BOTH
/// the Boa path and the no-`javascript` regex fallback share one bound.
const MAX_WAIT_MS: u64 = 60_000;

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
                use rand::Rng;
                rand::thread_rng().gen_range(*min_ms..=*max_ms)
            }
            // Both spellings of a JS-function wait run the identical path (issue #608): same Boa
            // execution, same cap, same loud fallback.
            WaitBehavior::Function(js_func) | WaitBehavior::Inject { inject: js_func } => {
                // Parse JavaScript function and execute
                // Format: "function() { return Math.floor(Math.random() * 100) + 50; }"
                // A failed/unusable wait function falls back to 100ms — but loudly, so it is
                // distinguishable from a genuine 100ms wait (B4, issue #355).
                // Cap here (issue #490) so both the Boa path and the no-`javascript` regex
                // fallback share the one bound — the fallback used to return the raw parsed value.
                Self::execute_js_wait_function(js_func)
                    .map(|ms| ms.min(MAX_WAIT_MS))
                    .unwrap_or_else(|| {
                        tracing::warn!(
                            target: "rift::script",
                            "wait function produced no usable delay; falling back to 100ms"
                        );
                        100
                    })
            }
        }
    }

    /// Execute a JavaScript wait function.
    ///
    /// When the `javascript` feature is enabled, this actually runs the function body on a
    /// bounded Boa `Context` (issue #355 Item 6) rather than pattern-matching a couple of known
    /// `Math.random` shapes — so any wait function (not just the ones the old regex recognized)
    /// produces a correct value. Without the feature, falls back to the original regex-based
    /// extraction so `--no-default-features` still builds and works for the common patterns.
    fn execute_js_wait_function(js_func: &str) -> Option<u64> {
        let trimmed = js_func.trim();
        if !trimmed.starts_with("function") {
            return None;
        }

        #[cfg(feature = "javascript")]
        {
            if let Some(ms) = Self::execute_js_wait_function_boa(trimmed) {
                return Some(ms);
            }
        }

        Self::execute_js_wait_function_regex(trimmed)
    }

    /// Run the wait function body for real on a bounded Boa `Context`: evaluate `(<js_func>)()`
    /// and coerce the numeric result to a `u64` delay, floored and capped at 60s so a
    /// pathological/negative/huge result can't turn into an enormous or nonsensical sleep.
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
        let n = match result.to_number(&mut context) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    target: "rift::script",
                    "wait function returned a non-numeric result: {e}"
                );
                return None;
            }
        };
        if !n.is_finite() {
            tracing::warn!(
                target: "rift::script",
                "wait function returned a non-finite number: {n}"
            );
            return None;
        }
        let ms = n.max(0.0).floor() as u64;
        Some(ms.min(MAX_WAIT_MS))
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
                use rand::Rng;
                // Extract multiplier and offset using the cached constant patterns.
                if let Some(caps) = WAIT_FLOOR_OFFSET_RE.captures(&body) {
                    let range = caps.get(1)?.as_str().parse::<u64>().ok()?;
                    let offset = caps.get(2)?.as_str().parse::<u64>().ok()?;
                    return Some(rand::thread_rng().gen_range(offset..=offset + range));
                }

                // Simpler pattern: Math.random() * N
                if let Some(caps) = WAIT_RANDOM_RE.captures(&body) {
                    let range = caps.get(1)?.as_str().parse::<u64>().ok()?;
                    return Some(rand::thread_rng().gen_range(0..=range));
                }
            }

            // Try to parse as a simple number. Parse signed so a negative literal clamps to 0
            // (matching the Boa path) instead of failing to parse and dropping to the 100ms
            // fallback (issue #490).
            body.trim().parse::<i64>().ok().map(|v| v.max(0) as u64)
        } else {
            None
        }
    }

    /// Parse Solo wait pattern:
    /// var min = Math.ceil(N); var max = Math.floor(M); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait;
    fn parse_solo_wait_pattern(body: &str) -> Option<u64> {
        use rand::Rng;

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
            Some(rand::thread_rng().gen_range(min_val..=max_val))
        } else {
            Some(min_val)
        }
    }
}

/// Extract function body from JavaScript function string
fn extract_function_body(js_func: &str) -> Option<String> {
    let start = js_func.find('{')?;
    let end = js_func.rfind('}')?;
    if start < end {
        Some(js_func[start + 1..end].trim().to_string())
    } else {
        None
    }
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

    // AC 608-2: the 60s cap and the loud fallback apply to the object form too (issues #355/#490).
    #[test]
    fn wait_inject_shares_cap_and_fallback() {
        let huge = WaitBehavior::Inject {
            inject: "function() { return 999999999; }".to_string(),
        };
        assert!(
            huge.get_duration_ms() <= MAX_WAIT_MS,
            "the object form must share the 60s cap"
        );

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

    #[test]
    fn test_wait_behavior_serde() {
        let yaml = "100";
        let wait: WaitBehavior = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(wait, WaitBehavior::Fixed(100)));

        let yaml = "min: 100\nmax: 200";
        let wait: WaitBehavior = serde_yaml::from_str(yaml).unwrap();
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

    // A huge return value is capped rather than producing an unbounded sleep.
    #[test]
    fn wait_function_huge_value_is_capped() {
        let wait = WaitBehavior::Function("function() { return 999999999; }".to_string());
        assert_eq!(wait.get_duration_ms(), 60_000);
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
        // None -> 100ms), matching the Boa path. The huge-value cap is applied at the
        // get_duration_ms boundary (see wait_function_huge_value_is_capped, which exercises this
        // fallback under --no-default-features).
        assert_eq!(
            WaitBehavior::execute_js_wait_function_regex("function() { return -5; }"),
            Some(0)
        );
    }
}
