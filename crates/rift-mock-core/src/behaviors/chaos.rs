//! Chaos behavior (issue #1850), as Mountebank 2.10.0 runs it (`behaviors.js` `chaos`): delay a
//! share of responses by up to `maxLatencyMs`, then replace a share of them with an error status
//! and an empty body, keeping the headers. It exists only in the `behaviors` array form:
//! Mountebank's `compatibility.js` upcast of `_behaviors` ignores it, so the object form drops it
//! (`compile_behaviors`).

use serde_json::Value;
use std::time::Duration;

/// The status an injected error is served with: Mountebank assigns `errorStatusCode` as written and
/// Node's `writeHead` reads it, so a value that is not an integer in `u16` is kept as its text and
/// resolved when the response is written, where one outside 100–999 answers Node's invalid-status
/// `500`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChaosStatus {
    Code(u16),
    Text(String),
}

/// One `chaos` element's configuration, with Mountebank's run-time defaults applied.
#[derive(Debug, Clone, PartialEq)]
pub struct ChaosBehavior {
    /// `errorRate || 0`: a hit is a draw below it, so `0` or less never fires and above `1` always.
    pub error_rate: f64,
    /// `errorStatusCode || 500`.
    pub error_status: ChaosStatus,
    /// `latencyRate || 0`.
    pub latency_rate: f64,
    /// `maxLatencyMs` when it is a number, else `1000`; `0` or less injects no latency.
    pub max_latency_ms: f64,
}

/// What one request's rolls decided.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub(crate) struct ChaosRoll {
    pub(crate) delay: Option<Duration>,
    pub(crate) error: bool,
}

/// The fields a config door checks, in the order Mountebank's validation spec lists them, with its
/// context for each and whether it must be non-negative.
const FIELDS: [(&str, &str, bool); 4] = [
    (
        "errorRate",
        "probability between 0 and 1 of injecting an error response",
        false,
    ),
    (
        "errorStatusCode",
        "the status code to return when injecting an error (defaults to 500)",
        true,
    ),
    (
        "latencyRate",
        "probability between 0 and 1 of injecting random latency",
        false,
    ),
    (
        "maxLatencyMs",
        "maximum latency in milliseconds when injecting",
        true,
    ),
];

impl ChaosBehavior {
    /// The behavior a `chaos` value configures. `None` for a value that is not an object, which
    /// Mountebank reads no field of. Never fails: a field of the wrong type takes its default, so a
    /// program stored before a door checked these fields (engines up to v0.25.0 kept `chaos` as an
    /// inert custom key) still replays.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        let config = value.as_object()?;
        let number = |key: &str| config.get(key).and_then(Value::as_f64);
        Some(Self {
            error_rate: number("errorRate").unwrap_or(0.0),
            error_status: error_status(config.get("errorStatusCode")),
            latency_rate: number("latencyRate").unwrap_or(0.0),
            max_latency_ms: number("maxLatencyMs").unwrap_or(1000.0),
        })
    }

    /// Roll for one request, drawing uniform values in `[0, 1)` from `draw` in Mountebank's order:
    /// the latency roll, the delay only on a hit, then the error roll. A rate that cannot fire (or a
    /// `maxLatencyMs` of `0` or less) draws nothing, so a seeded stream (`_rift.scriptEngine.seed`)
    /// replays the same hits.
    pub(crate) fn roll(&self, mut draw: impl FnMut() -> f64) -> ChaosRoll {
        let delay =
            (self.latency_rate > 0.0 && self.max_latency_ms > 0.0 && draw() < self.latency_rate)
                // A float-to-int `as` saturates, so a huge `maxLatencyMs` is the longest sleep, as
                // a huge `wait` is. Node instead runs a `setTimeout` past 2^31-1 ms after 1 ms.
                .then(|| Duration::from_millis((draw() * self.max_latency_ms).floor() as u64));
        let error = self.error_rate > 0.0 && draw() < self.error_rate;
        ChaosRoll { delay, error }
    }
}

/// `errorStatusCode || 500`, as JavaScript reads it: a falsy value is `500`.
fn error_status(value: Option<&Value>) -> ChaosStatus {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => ChaosStatus::Code(500),
        Some(Value::Number(n)) => match n.as_u64().and_then(|n| u16::try_from(n).ok()) {
            Some(0) => ChaosStatus::Code(500),
            Some(code) => ChaosStatus::Code(code),
            None if n.as_f64() == Some(0.0) => ChaosStatus::Code(500),
            None => ChaosStatus::Text(n.to_string()),
        },
        Some(Value::String(s)) if s.is_empty() => ChaosStatus::Code(500),
        Some(Value::String(s)) => ChaosStatus::Text(s.clone()),
        Some(other) => ChaosStatus::Text(other.to_string()),
    }
}

/// Refuse, at a config door, a `chaos` value Mountebank 2.10.0's `behaviorsValidator.js` refuses: a
/// field present with a value that is not a number (`null` included), or an `errorStatusCode` or
/// `maxLatencyMs` below `0`. A non-integer passes, as there. Only an object is checked: Mountebank
/// reads no field of anything else. The first field in spec order is reported.
///
/// # Errors
/// Mountebank's message for the first refused field.
pub(crate) fn check_chaos(value: &Value) -> Result<(), String> {
    let Some(config) = value.as_object() else {
        return Ok(());
    };
    for (field, context, non_negative) in FIELDS {
        let Some(value) = config.get(field) else {
            continue;
        };
        let Some(number) = value.as_f64() else {
            return Err(format!(
                "chaos behavior \"{field}\" field must be a number, representing {context}"
            ));
        };
        if non_negative && number < 0.0 {
            return Err(format!(
                "chaos behavior \"{field}\" field must be an integer greater than or equal to 0"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> ChaosBehavior {
        ChaosBehavior::from_value(&value).expect("an object configures chaos")
    }

    #[test]
    fn defaults_are_mountebanks() {
        assert_eq!(
            parse(json!({})),
            ChaosBehavior {
                error_rate: 0.0,
                error_status: ChaosStatus::Code(500),
                latency_rate: 0.0,
                max_latency_ms: 1000.0,
            }
        );
        // A field of the wrong type, as an older engine stored it, takes its default.
        assert_eq!(
            parse(json!({"errorRate": "x", "maxLatencyMs": null, "bogus": 1})),
            parse(json!({}))
        );
    }

    #[test]
    fn a_non_object_configures_nothing() {
        for value in [json!("x"), json!(5), json!([]), json!(null)] {
            assert_eq!(ChaosBehavior::from_value(&value), None, "{value}");
        }
    }

    #[test]
    fn the_error_status_is_kept_as_written_and_a_falsy_one_is_500() {
        let status = |code: Value| parse(json!({ "errorStatusCode": code })).error_status;
        assert_eq!(status(json!(503)), ChaosStatus::Code(503));
        assert_eq!(status(json!(0)), ChaosStatus::Code(500));
        assert_eq!(status(json!(0.0)), ChaosStatus::Code(500));
        assert_eq!(status(json!("")), ChaosStatus::Code(500));
        assert_eq!(status(json!(99)), ChaosStatus::Code(99));
        assert_eq!(status(json!(1.5)), ChaosStatus::Text("1.5".into()));
        assert_eq!(status(json!(70_000)), ChaosStatus::Text("70000".into()));
        assert_eq!(status(json!("503")), ChaosStatus::Text("503".into()));
    }

    #[test]
    fn rolls_draw_latency_then_delay_then_error() {
        let chaos = parse(json!({"errorRate": 0.5, "latencyRate": 0.5, "maxLatencyMs": 100}));
        let mut draws = [0.1, 0.25, 0.9].into_iter();
        let roll = chaos.roll(|| draws.next().expect("three draws"));
        assert_eq!(
            roll,
            ChaosRoll {
                delay: Some(Duration::from_millis(25)),
                error: false
            }
        );
        assert_eq!(draws.next(), None);
        // A latency miss draws no delay.
        let mut draws = [0.7, 0.2].into_iter();
        let roll = chaos.roll(|| draws.next().expect("two draws"));
        assert_eq!(
            roll,
            ChaosRoll {
                delay: None,
                error: true
            }
        );
    }

    #[test]
    fn a_rate_or_max_that_cannot_fire_draws_nothing() {
        for value in [
            json!({}),
            json!({"errorRate": -1, "latencyRate": 0}),
            json!({"latencyRate": 1, "maxLatencyMs": 0}),
        ] {
            let roll = parse(value.clone()).roll(|| panic!("{value} must not draw"));
            assert_eq!(
                roll,
                ChaosRoll {
                    delay: None,
                    error: false
                }
            );
        }
        // A rate above 1 always fires.
        assert!(parse(json!({"errorRate": 2})).roll(|| 0.999).error);
    }

    #[test]
    fn the_door_refuses_in_mountebanks_words() {
        let refused = |value: Value| check_chaos(&value).expect_err("refused");
        assert_eq!(
            refused(json!({"errorRate": null})),
            "chaos behavior \"errorRate\" field must be a number, representing probability \
             between 0 and 1 of injecting an error response"
        );
        assert_eq!(
            refused(json!({"maxLatencyMs": -1})),
            "chaos behavior \"maxLatencyMs\" field must be an integer greater than or equal to 0"
        );
        // The first field in spec order, whatever the written order.
        assert!(
            refused(json!({"maxLatencyMs": "x", "errorStatusCode": true}))
                .contains("\"errorStatusCode\" field must be a number")
        );
        for admitted in [
            json!("x"),
            json!([]),
            json!({"bogus": 1}),
            json!({"errorRate": -1, "errorStatusCode": 1.5, "maxLatencyMs": 0}),
        ] {
            assert_eq!(check_chaos(&admitted), Ok(()), "{admitted}");
        }
    }
}
