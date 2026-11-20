//! What `rift wasm check` compares (issue #1759): the fields of a recorded answer and a component's
//! answer, and how. Pure functions, so the comparison is tested apart from running components.

use serde_json::Value;

/// One field to compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// The status code.
    Status,
    /// The whole body: structurally when both sides parse as JSON, else as bytes.
    Body,
    /// One value in a JSON body, by `$`-path (`body.$.items[0].id`).
    BodyPath(String),
    /// A header's values, by name (case-insensitive), joined with `, `.
    Header(String),
}

impl Field {
    /// How the field is named in output and in `--fields`.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Status => "status".into(),
            Self::Body => "body".into(),
            Self::BodyPath(path) => format!("body.{path}"),
            Self::Header(name) => format!("headers.{name}"),
        }
    }
}

/// The default comparison: status and body. Headers vary between a recording and a component for
/// reasons that are rarely the behaviour under test (dates, server names), so they are opt-in.
#[must_use]
pub fn default_fields() -> Vec<Field> {
    vec![Field::Status, Field::Body]
}

/// `--fields status,headers.content-type,body.$.seq`.
pub fn parse_fields(spec: &str) -> Result<Vec<Field>, String> {
    let mut fields = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let field = if part == "status" {
            Field::Status
        } else if part == "body" {
            Field::Body
        } else if let Some(path) = part.strip_prefix("body.") {
            if !path.starts_with('$') {
                return Err(format!(
                    "--fields: a body field is a $-path, as in body.$.items[0].id (got {part:?})"
                ));
            }
            Field::BodyPath(path.to_owned())
        } else if let Some(name) = part.strip_prefix("headers.") {
            if name.is_empty() {
                return Err(format!(
                    "--fields: a header field names the header (got {part:?})"
                ));
            }
            Field::Header(name.to_ascii_lowercase())
        } else {
            return Err(format!(
                "--fields: unknown field {part:?}; expected status, body, body.$<path> or headers.<name>"
            ));
        };
        if !fields.contains(&field) {
            fields.push(field);
        }
    }
    if fields.is_empty() {
        return Err("--fields: name at least one field".into());
    }
    Ok(fields)
}

/// An answer, recorded or the component's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// One field that differs.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Mismatch {
    pub field: String,
    pub expected: Value,
    pub actual: Value,
}

fn as_json(body: &[u8]) -> Option<Value> {
    // Domain-optional parse: a body may legitimately not be JSON; then it is compared as text.
    serde_json::from_slice(body).ok()
}

/// A body as the report shows it: its JSON when it is JSON, else its text (lossy for non-UTF-8).
fn shown(body: &[u8]) -> Value {
    as_json(body).unwrap_or_else(|| Value::String(String::from_utf8_lossy(body).into_owned()))
}

fn header(answer: &Answer, name: &str) -> Value {
    let values: Vec<&str> = answer
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect();
    if values.is_empty() {
        Value::Null
    } else {
        Value::String(values.join(", "))
    }
}

/// What a `$`-path finds in a body; a missing path (or a body that is not JSON) is reported as
/// `"<absent>"`, so a path that names nothing never matches quietly (a typo in `--fields`).
fn at_path(body: &[u8], path: &str) -> Option<Value> {
    as_json(body)
        .and_then(|json| rift_mock_core::extensions::template_fn::eval_jsonpath(&json, path).ok())
}

const ABSENT: &str = "<absent>";

/// Every field of `fields` where `actual` differs from `expected`.
#[must_use]
pub fn diff(fields: &[Field], expected: &Answer, actual: &Answer) -> Vec<Mismatch> {
    fields
        .iter()
        .filter_map(|field| {
            let (want, got) = match field {
                Field::Status => (Value::from(expected.status), Value::from(actual.status)),
                Field::Body => match (as_json(&expected.body), as_json(&actual.body)) {
                    (Some(want), Some(got)) => (want, got),
                    _ if expected.body == actual.body => return None,
                    _ => (shown(&expected.body), shown(&actual.body)),
                },
                Field::BodyPath(path) => {
                    match (at_path(&expected.body, path), at_path(&actual.body, path)) {
                        (Some(want), Some(got)) => (want, got),
                        (want, got) => {
                            // Absent from the recording is drift whatever the component says: the
                            // field asked for is not in the evidence.
                            let shown = |v: Option<Value>| v.unwrap_or_else(|| Value::from(ABSENT));
                            return Some(Mismatch {
                                field: field.name(),
                                expected: shown(want),
                                actual: shown(got),
                            });
                        }
                    }
                }
                Field::Header(name) => (header(expected, name), header(actual, name)),
            };
            (want != got).then(|| Mismatch {
                field: field.name(),
                expected: want,
                actual: got,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer(status: u16, headers: &[(&str, &str)], body: &str) -> Answer {
        Answer {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn fields_parse_and_refuse_what_they_cannot_read() {
        assert_eq!(
            parse_fields("status, headers.Content-Type,body.$.seq,status").expect("parses"),
            vec![
                Field::Status,
                Field::Header("content-type".into()),
                Field::BodyPath("$.seq".into())
            ]
        );
        for bad in ["", "statuss", "body.seq", "headers."] {
            assert!(parse_fields(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn json_bodies_compare_by_structure_and_others_by_bytes() {
        let fields = default_fields();
        let a = answer(200, &[], r#"{"count": 1, "ok": true}"#);
        let b = answer(200, &[], r#"{"ok":true,"count":1}"#);
        assert!(
            diff(&fields, &a, &b).is_empty(),
            "same JSON, other spacing and order"
        );
        let c = answer(200, &[], r#"{"ok":true,"count":2}"#);
        assert_eq!(
            diff(&fields, &a, &c),
            vec![Mismatch {
                field: "body".into(),
                expected: json!({"count": 1, "ok": true}),
                actual: json!({"count": 2, "ok": true}),
            }]
        );
        assert!(
            diff(
                &fields,
                &answer(200, &[], "plain"),
                &answer(200, &[], "plain")
            )
            .is_empty()
        );
        assert_eq!(
            diff(
                &fields,
                &answer(200, &[], "plain"),
                &answer(201, &[], "other")
            )
            .len(),
            2
        );
    }

    #[test]
    fn a_path_and_a_header_select_one_value() {
        let a = answer(
            200,
            &[("Content-Type", "application/json")],
            r#"{"seq":3,"t":1}"#,
        );
        let b = answer(
            200,
            &[("content-type", "application/json")],
            r#"{"seq":3,"t":2}"#,
        );
        let fields = parse_fields("headers.content-type,body.$.seq").expect("parses");
        assert!(
            diff(&fields, &a, &b).is_empty(),
            "the header's case and an unselected field"
        );
        let fields = parse_fields("body.$.t,headers.x-missing").expect("parses");
        let mismatches = diff(&fields, &a, &b);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].field, "body.$.t");
        // A path the recording does not have drifts, even when the component lacks it too.
        let typo = parse_fields("body.$.sqe").expect("parses");
        let mismatches = diff(&typo, &a, &b);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].expected, json!("<absent>"));
    }
}
