//! What one engine answered, and the differences between two answers.

use serde_json::Value;
use std::fmt;

/// Response headers that are transport or clock artefacts, never compared. Names are matched
/// case-insensitively. `Content-Length` / `Transfer-Encoding` are framing: the body itself is
/// compared byte for byte, which subsumes its length.
pub const IGNORED_HEADERS: &[&str] = &[
    "date",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
    "server",
];

/// Whether `name` is one of [`IGNORED_HEADERS`], ignoring case.
#[must_use]
pub fn is_ignored_header(name: &str) -> bool {
    IGNORED_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// Response headers as compared: names lower-cased, [`IGNORED_HEADERS`] dropped, arrival order and
/// repeated names kept, values port-mapped back to logical.
pub fn normalize_headers<'a, V: AsRef<str>>(
    headers: impl IntoIterator<Item = (&'a str, V)>,
    ports: &crate::ports::PortMap,
) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), ports.reverse_str(value.as_ref())))
        .filter(|(name, _)| !is_ignored_header(name))
        .collect()
}

/// One engine's answer to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Response {
        status: u16,
        /// Lower-cased names, in arrival order, ignored headers already removed.
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// The request did not produce a response (a fault closed the connection, a timeout). Only
    /// the coarse class is compared: the two engines' socket errors are not worded alike.
    Failed(FailureClass),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Connect,
    Timeout,
    /// The connection was closed or reset, or the response was malformed.
    Transport,
}

/// The kind of a difference; an allow-list entry names the class it permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffClass {
    /// The values differ.
    Value,
    /// A served JSON body has the same value but different text — and Rift's text is exactly the
    /// key-sorted compact serialisation (`docs/mountebank/responses.md`): key order and whitespace.
    JsonText,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Difference {
    /// Where: `step[2].status`, `step[2].header.content-type`, `step[2].body/a/0`,
    /// `stored[4545]/stubs/0/responses`, `replayable[4545]/...`.
    pub location: String,
    pub class: DiffClass,
    pub mountebank: String,
    pub rift: String,
}

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = match self.class {
            DiffClass::Value => "",
            DiffClass::JsonText => " (same JSON value, different text)",
        };
        write!(
            f,
            "{}{class}\n      mountebank: {}\n      rift:       {}",
            self.location,
            clip(&self.mountebank),
            clip(&self.rift)
        )
    }
}

fn clip(text: &str) -> String {
    const MAX: usize = 400;
    let text = &text.replace('\n', "\\n");
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX).collect();
    format!("{head}… ({} bytes)", text.len())
}

/// Compares two engines' answers to the same request. `json_bodies_as_values` is set for admin
/// responses, whose JSON key order is not a contract: their bodies are compared as parsed values.
#[must_use]
pub fn compare_observations(
    at: &str,
    mountebank: &Observation,
    rift: &Observation,
    json_bodies_as_values: bool,
) -> Vec<Difference> {
    let mut out = Vec::new();
    match (mountebank, rift) {
        (
            Observation::Response {
                status: mb_status,
                headers: mb_headers,
                body: mb_body,
            },
            Observation::Response {
                status: rift_status,
                headers: rift_headers,
                body: rift_body,
            },
        ) => {
            if mb_status != rift_status {
                out.push(value_diff(
                    format!("{at}.status"),
                    mb_status.to_string(),
                    rift_status.to_string(),
                ));
            }
            compare_headers(at, mb_headers, rift_headers, &mut out);
            compare_bodies(at, mb_body, rift_body, json_bodies_as_values, &mut out);
        }
        (mb, rift) if mb == rift => {}
        (mb, rift) => out.push(value_diff(
            format!("{at}.outcome"),
            describe(mb),
            describe(rift),
        )),
    }
    out
}

fn describe(observation: &Observation) -> String {
    match observation {
        Observation::Response { status, .. } => format!("response {status}"),
        Observation::Failed(class) => format!("no response ({class:?})"),
    }
}

fn compare_headers(
    at: &str,
    mb: &[(String, String)],
    rift: &[(String, String)],
    out: &mut Vec<Difference>,
) {
    let names: std::collections::BTreeSet<&str> = mb
        .iter()
        .chain(rift.iter())
        .map(|(name, _)| name.as_str())
        .collect();
    for name in names {
        let values = |headers: &[(String, String)]| -> Vec<String> {
            headers
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .collect()
        };
        let (mb_values, rift_values) = (values(mb), values(rift));
        if mb_values != rift_values {
            out.push(value_diff(
                format!("{at}.header.{name}"),
                render_header(&mb_values),
                render_header(&rift_values),
            ));
        }
    }
}

fn render_header(values: &[String]) -> String {
    if values.is_empty() {
        "(absent)".to_string()
    } else {
        values.join(" | ")
    }
}

fn compare_bodies(at: &str, mb: &[u8], rift: &[u8], as_values: bool, out: &mut Vec<Difference>) {
    let parsed = (
        serde_json::from_slice::<Value>(mb),
        serde_json::from_slice::<Value>(rift),
    );
    if let (Ok(mb_json), Ok(rift_json)) = parsed {
        let before = out.len();
        compare_json(&format!("{at}.body"), &mb_json, &rift_json, out);
        if out.len() == before && !as_values && mb != rift {
            let canonical_text =
                serde_json::to_vec(&rift_json).expect("a serde_json::Value always serialises");
            let class = if canonical_text == rift {
                DiffClass::JsonText
            } else {
                DiffClass::Value
            };
            out.push(Difference {
                location: format!("{at}.body"),
                class,
                mountebank: String::from_utf8_lossy(mb).into_owned(),
                rift: String::from_utf8_lossy(rift).into_owned(),
            });
        }
        return;
    }
    if mb != rift {
        out.push(value_diff(
            format!("{at}.body"),
            String::from_utf8_lossy(mb).into_owned(),
            String::from_utf8_lossy(rift).into_owned(),
        ));
    }
}

/// Structural JSON diff; each leaf difference is reported at its JSON-pointer location.
pub fn compare_json(at: &str, mb: &Value, rift: &Value, out: &mut Vec<Difference>) {
    match (mb, rift) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let path = format!("{at}/{}", pointer_escape(key));
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => compare_json(&path, x, y, out),
                    (x, y) => out.push(value_diff(path, render(x), render(y))),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for index in 0..a.len().max(b.len()) {
                let path = format!("{at}/{index}");
                match (a.get(index), b.get(index)) {
                    (Some(x), Some(y)) => compare_json(&path, x, y, out),
                    (x, y) => out.push(value_diff(path, render(x), render(y))),
                }
            }
        }
        (a, b) if a == b => {}
        (a, b) => out.push(value_diff(at.to_string(), a.to_string(), b.to_string())),
    }
}

fn render(value: Option<&Value>) -> String {
    value.map_or_else(|| "(absent)".to_string(), Value::to_string)
}

fn pointer_escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn value_diff(location: String, mountebank: String, rift: String) -> Difference {
    Difference {
        location,
        class: DiffClass::Value,
        mountebank,
        rift,
    }
}

/// A unified diff of two canonical documents, for the failure message.
#[must_use]
pub fn unified(mb: &Value, rift: &Value) -> String {
    let pretty =
        |v: &Value| serde_json::to_string_pretty(v).expect("a serde_json::Value always serialises");
    let (a, b) = (pretty(mb), pretty(rift));
    similar::TextDiff::from_lines(&a, &b)
        .unified_diff()
        .context_radius(3)
        .header("mountebank", "rift")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Observation {
        Observation::Response {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn locations(diffs: &[Difference]) -> Vec<(&str, DiffClass)> {
        diffs
            .iter()
            .map(|d| (d.location.as_str(), d.class))
            .collect()
    }

    #[test]
    fn headers_are_folded_filtered_and_keep_repeats_in_order() {
        let ports = crate::ports::PortMap::new([(4545, 50001)], 50000);
        let normalized = normalize_headers(
            [
                ("Date", "Thu, 08 Oct 2026 00:00:00 GMT"),
                ("Set-Cookie", "a=1"),
                ("SERVER", "x"),
                ("set-cookie", "b=2"),
                ("Location", "http://localhost:50000/imposters/50001"),
            ],
            &ports,
        );
        assert_eq!(
            normalized,
            vec![
                ("set-cookie".to_string(), "a=1".to_string()),
                ("set-cookie".to_string(), "b=2".to_string()),
                (
                    "location".to_string(),
                    "http://localhost:2525/imposters/4545".to_string()
                ),
            ]
        );
    }

    #[test]
    fn repeated_header_values_differ_by_order_and_by_count() {
        let mb = response(200, &[("set-cookie", "a=1"), ("set-cookie", "b=2")], "");
        let swapped = response(200, &[("set-cookie", "b=2"), ("set-cookie", "a=1")], "");
        let one = response(200, &[("set-cookie", "a=1")], "");
        for rift in [&swapped, &one] {
            let diffs = compare_observations("step[0]", &mb, rift, false);
            assert_eq!(
                locations(&diffs),
                vec![("step[0].header.set-cookie", DiffClass::Value)]
            );
        }
    }

    #[test]
    fn json_text_detection_holds_for_nested_documents() {
        let mb = response(200, &[], "{\n    \"z\": [{\"b\": 1, \"a\": 2}]\n}");
        let rift = response(200, &[], r#"{"z":[{"a":2,"b":1}]}"#);
        let diffs = compare_observations("step[0]", &mb, &rift, false);
        assert_eq!(
            locations(&diffs),
            vec![("step[0].body", DiffClass::JsonText)]
        );
        let text = response(200, &[], "not json");
        let diffs = compare_observations("step[0]", &mb, &text, false);
        assert_eq!(locations(&diffs), vec![("step[0].body", DiffClass::Value)]);
    }

    #[test]
    fn identical_answers_have_no_difference() {
        let a = response(200, &[("x-a", "1")], "hello");
        assert!(compare_observations("step[0]", &a, &a.clone(), false).is_empty());
    }

    #[test]
    fn status_header_and_body_differences_are_located() {
        let mb = response(200, &[("x-a", "1"), ("x-only-mb", "m")], "hello");
        let rift = response(201, &[("x-a", "2")], "hullo");
        let diffs = compare_observations("step[3]", &mb, &rift, false);
        assert_eq!(
            locations(&diffs),
            vec![
                ("step[3].status", DiffClass::Value),
                ("step[3].header.x-a", DiffClass::Value),
                ("step[3].header.x-only-mb", DiffClass::Value),
                ("step[3].body", DiffClass::Value),
            ]
        );
        assert_eq!(diffs[2].rift, "(absent)");
    }

    #[test]
    fn json_bodies_are_diffed_by_pointer() {
        let mb = response(200, &[], r#"{"a":[1,{"b":"x"}],"c":true}"#);
        let rift = response(200, &[], r#"{"a":[1,{"b":"y"}]}"#);
        let diffs = compare_observations("step[0]", &mb, &rift, false);
        assert_eq!(
            locations(&diffs),
            vec![
                ("step[0].body/a/1/b", DiffClass::Value),
                ("step[0].body/c", DiffClass::Value),
            ]
        );
    }

    #[test]
    fn json_text_is_its_own_class_only_when_rift_serves_the_sorted_compact_form() {
        let mb = response(200, &[], r#"{"f":0.1,"big":1}"#);
        let sorted = response(200, &[], r#"{"big":1,"f":0.1}"#);
        let diffs = compare_observations("step[0]", &mb, &sorted, false);
        assert_eq!(
            locations(&diffs),
            vec![("step[0].body", DiffClass::JsonText)]
        );

        let spaced = response(200, &[], r#"{"big": 1, "f": 0.1}"#);
        let diffs = compare_observations("step[0]", &mb, &spaced, false);
        assert_eq!(locations(&diffs), vec![("step[0].body", DiffClass::Value)]);

        assert!(compare_observations("step[0]", &mb, &sorted, true).is_empty());
    }

    #[test]
    fn failures_compare_by_class() {
        let reset = Observation::Failed(FailureClass::Transport);
        assert!(compare_observations("step[0]", &reset, &reset.clone(), false).is_empty());
        let diffs = compare_observations("step[0]", &reset, &response(200, &[], ""), false);
        assert_eq!(
            locations(&diffs),
            vec![("step[0].outcome", DiffClass::Value)]
        );
        assert_eq!(diffs[0].rift, "response 200");
    }
}
