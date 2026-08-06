//! Mountebank wire-shape tolerance, shared across crates (issue #936).
//!
//! Mountebank accepts several spellings of the same value — a status code as a number *or* a
//! numeric string, a header as one value *or* an array, a header value as any JSON scalar. Those
//! rules are a property of the wire format, not of any one subsystem, so they live here rather
//! than inside the imposter types: the imposter stub path and the intercept rule schema both
//! parse the same JSON and must agree about what it means. They previously lived in
//! `rift-mock-core` as `pub(crate)`, which is exactly why the intercept path drifted (issue #933
//! for `body`, this issue for `statusCode`/`headers`).

/// Serde for multi-value headers (issue #238). Accepts the Mountebank-style `"k": "v"` *and*
/// `"k": ["v1", "v2"]` on the wire; serializes a single value back as a plain string and multiple
/// values as an array, so existing single-value consumers are unaffected.
pub mod multi_value_headers {
    use serde::Deserialize;
    use serde::de::Deserializer;
    use serde::ser::{SerializeMap, Serializer};
    use std::collections::HashMap;

    pub fn serialize<S: Serializer>(
        headers: &HashMap<String, Vec<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(headers.len()))?;
        for (key, values) in headers {
            match values.as_slice() {
                [] => continue, // a key with no values would emit no header line; omit it
                [single] => map.serialize_entry(key, single)?,
                many => map.serialize_entry(key, many)?,
            }
        }
        map.end()
    }

    /// A single header value on the wire. Mountebank tolerates non-string scalars — its recorders
    /// routinely emit `"Content-Length": 124` (a JSON number) and `"X-Flag": true` — and coerces
    /// them to their string form. Rift matches that so real recorded imposters load unchanged
    /// (issue #754); previously a numeric/bool value failed both `OneOrMany` variants and rejected
    /// the whole imposter with a 400.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scalar {
        Str(String),
        Num(serde_json::Number),
        Bool(bool),
    }

    impl Scalar {
        fn into_string(self) -> String {
            match self {
                Scalar::Str(s) => s,
                Scalar::Num(n) => n.to_string(),
                Scalar::Bool(b) => b.to_string(),
            }
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<String, Vec<String>>, D::Error> {
        // Order matters for `#[serde(untagged)]`: a scalar can never match `Many` and an array can
        // never match `One`, so either order is sound — `One` first keeps the common case first.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(Scalar),
            Many(Vec<Scalar>),
        }
        let raw = HashMap::<String, OneOrMany>::deserialize(deserializer)?;
        Ok(raw
            .into_iter()
            .map(|(k, v)| match v {
                OneOrMany::One(s) => (k, vec![s.into_string()]),
                OneOrMany::Many(v) => (k, v.into_iter().map(Scalar::into_string).collect()),
            })
            .collect())
    }
}

use serde::Deserialize;

/// Parse a JSON `statusCode` value that may be a number or a (numeric) string.
fn parse_status_code_value<E: serde::de::Error>(value: serde_json::Value) -> Result<u16, E> {
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| E::custom("invalid status code number")),
        serde_json::Value::String(s) => s
            .parse::<u16>()
            .map_err(|_| E::custom(format!("invalid status code string: {s}"))),
        _ => Err(E::custom("statusCode must be a number or string")),
    }
}

/// Deserialize statusCode from either a number or a string
pub fn deserialize_status_code<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    parse_status_code_value(serde_json::Value::deserialize(deserializer)?)
}

/// Deserialize an optional top-level `statusCode` (flat response form, issue #304), reusing the
/// number-or-string parsing. Only invoked when the field is present; a `null` is treated as
/// absent (`None`) so a stray null on a non-flat response stays accepted as before.
pub fn deserialize_optional_status_code<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(None),
        value => parse_status_code_value(value).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::collections::HashMap;

    /// Exercises the helpers through serde attributes, the only way they are ever reached — a
    /// direct call would not prove the `#[serde(with = ...)]` wiring still resolves.
    #[derive(Deserialize)]
    struct HeadersIn {
        #[serde(default, deserialize_with = "multi_value_headers::deserialize")]
        headers: HashMap<String, Vec<String>>,
    }

    #[derive(Serialize)]
    struct HeadersOut {
        #[serde(serialize_with = "multi_value_headers::serialize")]
        headers: HashMap<String, Vec<String>>,
    }

    #[derive(Deserialize)]
    struct StatusIn {
        #[serde(deserialize_with = "deserialize_status_code")]
        status: u16,
    }

    #[derive(Deserialize)]
    struct OptionalStatusIn {
        #[serde(default, deserialize_with = "deserialize_optional_status_code")]
        status: Option<u16>,
    }

    #[test]
    fn headers_accept_a_bare_string_or_an_array() {
        let one: HeadersIn = serde_json::from_str(r#"{"headers":{"X-One":"v"}}"#).unwrap();
        assert_eq!(one.headers["X-One"], vec!["v".to_string()]);

        let many: HeadersIn =
            serde_json::from_str(r#"{"headers":{"Set-Cookie":["a","b"]}}"#).unwrap();
        assert_eq!(
            many.headers["Set-Cookie"],
            vec!["a".to_string(), "b".to_string()]
        );
    }

    // Issue #754: recorded imposters carry non-string scalars; they coerce rather than 400.
    #[test]
    fn headers_coerce_numeric_and_bool_scalars() {
        let r: HeadersIn = serde_json::from_str(
            r#"{"headers":{"Content-Length":124,"X-Flag":true,"X-Ratio":1.5,"X-Multi":[200,"x",false]}}"#,
        )
        .expect("numeric/bool header values must be accepted (mb parity)");
        assert_eq!(r.headers["Content-Length"], vec!["124".to_string()]);
        assert_eq!(r.headers["X-Flag"], vec!["true".to_string()]);
        assert_eq!(r.headers["X-Ratio"], vec!["1.5".to_string()]);
        assert_eq!(
            r.headers["X-Multi"],
            vec!["200".to_string(), "x".to_string(), "false".to_string()]
        );
    }

    #[test]
    fn headers_serialize_single_as_string_many_as_array_and_omit_empty() {
        let out = HeadersOut {
            headers: HashMap::from([
                ("X-One".to_string(), vec!["v".to_string()]),
                (
                    "Set-Cookie".to_string(),
                    vec!["a".to_string(), "b".to_string()],
                ),
                ("X-Empty".to_string(), vec![]),
            ]),
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["headers"]["X-One"], serde_json::json!("v"));
        assert_eq!(v["headers"]["Set-Cookie"], serde_json::json!(["a", "b"]));
        assert!(
            v["headers"].get("X-Empty").is_none(),
            "a key with no values emits no header line, so it is omitted"
        );
    }

    #[test]
    fn status_code_accepts_a_number_or_a_numeric_string() {
        assert_eq!(
            serde_json::from_str::<StatusIn>(r#"{"status":404}"#)
                .unwrap()
                .status,
            404
        );
        assert_eq!(
            serde_json::from_str::<StatusIn>(r#"{"status":"404"}"#)
                .unwrap()
                .status,
            404
        );
    }

    #[test]
    fn status_code_rejects_junk_rather_than_defaulting() {
        for junk in [
            r#"{"status":"abc"}"#,
            r#"{"status":true}"#,
            // One past `u16::MAX`, in both spellings — the boundary the two parse paths
            // (`str::parse::<u16>` and `u16::try_from(u64)`) have to agree on.
            r#"{"status":65536}"#,
            r#"{"status":"65536"}"#,
            r#"{"status":-1}"#,
            // A non-integer number: `Number::as_u64` yields `None` rather than truncating.
            r#"{"status":200.5}"#,
        ] {
            assert!(
                serde_json::from_str::<StatusIn>(junk).is_err(),
                "{junk} must be an error, not a silent default"
            );
        }

        // …and the boundary itself is still accepted, so the rejections above are landing on the
        // right side of it.
        assert_eq!(
            serde_json::from_str::<StatusIn>(r#"{"status":65535}"#)
                .unwrap()
                .status,
            65535
        );
    }

    #[test]
    fn optional_status_code_treats_null_and_absent_as_none() {
        assert_eq!(
            serde_json::from_str::<OptionalStatusIn>(r#"{}"#)
                .unwrap()
                .status,
            None
        );
        assert_eq!(
            serde_json::from_str::<OptionalStatusIn>(r#"{"status":null}"#)
                .unwrap()
                .status,
            None
        );
        assert_eq!(
            serde_json::from_str::<OptionalStatusIn>(r#"{"status":"201"}"#)
                .unwrap()
                .status,
            Some(201)
        );
    }
}
