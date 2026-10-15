//! A Prometheus text exposition parsed into one value per series (port of `PromText.scala`, #192).
//!
//! Series are keyed by exact metric name and label set: a Prometheus-client sibling such as
//! `rift_requests_total_created` is never folded into `rift_requests_total` (the #1123 lesson).

use regex::Regex;
use std::collections::BTreeMap;
use std::fmt;

/// A metric name plus its full label set. Label order is irrelevant, so the labels are a map.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Series {
    pub name: String,
    pub labels: BTreeMap<String, String>,
}

impl fmt::Display for Series {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let labels: Vec<String> = self
            .labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect();
        write!(f, "{}{{{}}}", self.name, labels.join(","))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromText {
    pub series: BTreeMap<Series, f64>,
}

const SAMPLE_LINE: &str = r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{(.*)\})?\s+(\S+)(\s+\S+)?$";
const LABEL: &str = r#"([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)""#;

impl PromText {
    #[must_use]
    pub fn get(&self, series: &Series) -> Option<f64> {
        self.series.get(series).copied()
    }

    /// Parse exposition text. Comment and blank lines are skipped; a sample line that does not
    /// parse is an error, never dropped: a dropped series would read as absent, turning a presence
    /// assertion into a false failure or a delta into a false zero.
    pub fn parse(text: &str) -> Result<Self, String> {
        let compile = |re: &str| Regex::new(re).map_err(|e| format!("regex {re}: {e}"));
        let (sample_line, label) = (compile(SAMPLE_LINE)?, compile(LABEL)?);
        let mut series = BTreeMap::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (s, v) = sample(&sample_line, &label, line)?;
            series.insert(s, v);
        }
        Ok(Self { series })
    }
}

fn sample(sample_line: &Regex, label: &Regex, line: &str) -> Result<(Series, f64), String> {
    let caps = sample_line
        .captures(line)
        .ok_or_else(|| format!("unparseable sample line: {line}"))?;
    let labels = caps.get(3).map_or_else(BTreeMap::new, |l| {
        label
            .captures_iter(l.as_str())
            .map(|m| (m[1].to_string(), m[2].to_string()))
            .collect()
    });
    let value = &caps[4];
    let v = match value {
        "+Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        "NaN" => f64::NAN,
        other => other
            .parse::<f64>()
            .map_err(|_| format!("unparseable sample value in: {line}"))?,
    };
    Ok((
        Series {
            name: caps[1].to_string(),
            labels,
        },
        v,
    ))
}

/// A feature-file selector: `name{k=v,k2=v2}` with UNQUOTED values (a quoted value cannot sit
/// inside a Gherkin string argument), or a bare `name`.
pub fn selector(s: &str) -> Result<Series, String> {
    let t = s.trim();
    let Some(i) = t.find('{') else {
        return Ok(Series {
            name: t.to_string(),
            labels: BTreeMap::new(),
        });
    };
    let Some(body) = t[i + 1..].strip_suffix('}') else {
        return Err(format!("selector '{s}' has an unclosed '{{'"));
    };
    let mut labels = BTreeMap::new();
    for kv in body.split(',').map(str::trim).filter(|kv| !kv.is_empty()) {
        match kv.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                labels.insert(k.trim().to_string(), v.trim().to_string());
            }
            _ => return Err(format!("bad label '{kv}' in selector '{s}'")),
        }
    }
    Ok(Series {
        name: t[..i].to_string(),
        labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(name: &str, labels: &[(&str, &str)]) -> Series {
        Series {
            name: name.to_string(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    const TEXT: &str = "# HELP rift_requests_total Requests served
# TYPE rift_requests_total counter
rift_requests_total{method=\"GET\",status=\"200\"} 5
rift_requests_total_created{method=\"GET\",status=\"200\"} 1.7e9
rift_imposters_total 3
rift_latency_injected_ms_bucket{rule_id=\"4880\",le=\"+Inf\"} 2
";

    #[test]
    fn keys_series_by_exact_name_and_labels_never_folding_in_a_created_sibling() {
        let p = PromText::parse(TEXT).expect("valid exposition");
        assert_eq!(
            p.get(&series(
                "rift_requests_total",
                &[("method", "GET"), ("status", "200")]
            )),
            Some(5.0)
        );
        assert_eq!(p.get(&series("rift_imposters_total", &[])), Some(3.0));
        assert_eq!(p.series.len(), 4);
    }

    #[test]
    fn a_malformed_sample_line_is_an_error_not_a_dropped_series() {
        assert!(PromText::parse("rift_x{a=\"1\"} notanumber").is_err());
    }

    #[test]
    fn a_selector_takes_unquoted_label_values_in_any_order() {
        assert_eq!(
            selector("rift_faults_injected_total{type=tcp,rule_id=4882}"),
            Ok(series(
                "rift_faults_injected_total",
                &[("rule_id", "4882"), ("type", "tcp")]
            ))
        );
        assert_eq!(
            selector("rift_imposters_total"),
            Ok(series("rift_imposters_total", &[]))
        );
        assert!(selector("rift_x{a}").is_err());
        assert!(selector("rift_x{a=1").is_err());
    }
}
