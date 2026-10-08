//! JSON Schema carriers for the wire-shape tolerances in [`wire`](crate::wire) (issue #1342).
//!
//! A `#[serde(with = …)]` helper has no type for `schemars` to derive from, so each tolerance gets a
//! marker type here whose schema says what the helper accepts. A field names its carrier with
//! `#[schemars(with = "rift_types::schema::…")]`; the serde attribute next to it stays the source of
//! truth for what is read, and these must say the same thing — the corpus and docs validation gate
//! (`issue_1343_unread_keys.rs`) is what catches a carrier that drifts from its helper.

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::marker::PhantomData;

/// A numeric string within `u16`, with the optional `+` Rust's parser takes.
const U16_STRING: &str =
    r"^\+?0*(6553[0-5]|655[0-2][0-9]|65[0-4][0-9]{2}|6[0-4][0-9]{3}|[1-5][0-9]{4}|[0-9]{1,4})$";

/// The `jsonpath` selector object a predicate or a `predicateGenerators` entry carries.
#[must_use]
pub fn jsonpath_selector() -> Value {
    json!({
        "type": "object",
        "properties": { "selector": { "type": "string" } },
        "required": ["selector"],
        "additionalProperties": false
    })
}

/// The `xpath` selector object, with Mountebank's `ns` prefix-to-URI map.
#[must_use]
pub fn xpath_selector() -> Value {
    json!({
        "type": "object",
        "properties": {
            "selector": { "type": "string" },
            "ns": { "type": "object", "additionalProperties": { "type": "string" } }
        },
        "required": ["selector"],
        "additionalProperties": false
    })
}

/// `statusCode` as [`wire::deserialize_status_code`](crate::wire::deserialize_status_code) reads
/// it: a number, or a numeric string, both within `u16`.
#[derive(Debug)]
pub struct StatusCode;

impl JsonSchema for StatusCode {
    fn schema_name() -> Cow<'static, str> {
        "StatusCode".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "An HTTP status code, as a number or a numeric string.",
            "anyOf": [
                { "type": "integer", "minimum": 0, "maximum": 65535 },
                { "type": "string", "pattern": U16_STRING }
            ]
        })
    }
}

/// A header map as [`wire::multi_value_headers`](crate::wire::multi_value_headers) reads it: each
/// name holds one value or an array of them, and a value is any JSON scalar.
#[derive(Debug)]
pub struct MultiValueHeaders;

impl JsonSchema for MultiValueHeaders {
    fn schema_name() -> Cow<'static, str> {
        "MultiValueHeaders".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        let scalar = serde_json::json!({ "type": ["string", "number", "boolean"] });
        json_schema!({
            "description": "Header names to one value or an array of values. A number or a \
                            boolean value is written as its text.",
            "type": "object",
            "additionalProperties": {
                "anyOf": [scalar, { "type": "array", "items": scalar }]
            }
        })
    }
}

/// A header map as [`wire::single_value_headers`](crate::wire::single_value_headers) reads it: one
/// string per name, each name given once.
#[derive(Debug)]
pub struct SingleValueHeaders;

impl JsonSchema for SingleValueHeaders {
    fn schema_name() -> Cow<'static, str> {
        "SingleValueHeaders".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Header names to one string value each; a name given twice in \
                            different cases is refused.",
            "type": "object",
            "additionalProperties": { "type": "string" }
        })
    }
}

/// One `T`, an array of them, or `null` for none — the shape the one-or-many behavior
/// deserializers (`copy`, `lookup`, `shellTransform`) accept. Never constructed: it exists only to
/// be named in `#[schemars(with = …)]`.
pub struct OneOrMany<T>(PhantomData<T>);

impl<T: JsonSchema> JsonSchema for OneOrMany<T> {
    fn schema_name() -> Cow<'static, str> {
        format!("OneOrMany_{}", T::schema_name()).into()
    }

    fn schema_id() -> Cow<'static, str> {
        format!("rift_types::schema::OneOrMany<{}>", T::schema_id()).into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let item = generator.subschema_for::<T>();
        json_schema!({
            "anyOf": [item, { "type": "array", "items": item }, { "type": "null" }]
        })
    }
}

/// An unsigned integer as a number or a numeric string — `delayRange`'s `min`/`max`. The string
/// form's `u64` range is not expressed: the helper refuses a string past it, the schema does not.
#[derive(Debug)]
pub struct U64OrNumericString;

impl JsonSchema for U64OrNumericString {
    fn schema_name() -> Cow<'static, str> {
        "U64OrNumericString".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A non-negative integer, as a number or a numeric string.",
            "anyOf": [
                { "type": "integer", "minimum": 0 },
                { "type": "string", "pattern": "^\\+?[0-9]+$" }
            ]
        })
    }
}

/// One PEM string or an array of them — the imposter's `ca`, as Mountebank spells it.
#[derive(Debug)]
pub struct PemList;

impl JsonSchema for PemList {
    fn schema_name() -> Cow<'static, str> {
        "PemList".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "One PEM string or an array of them.",
            "anyOf": [
                { "type": "string" },
                { "type": "array", "items": { "type": "string" } }
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire;
    use schemars::schema_for;
    use serde::Deserialize;
    use std::collections::HashMap;

    /// Each carrier's verdict on a sample is the serde helper's verdict on the same bytes: the
    /// carrier is the only description of the helper a consumer sees, so the two have to agree —
    /// and the only way to know they do is to run the helper.
    fn agree<T: for<'de> Deserialize<'de>>(schema: &Value, samples: &[(Value, bool)]) {
        let validator = jsonschema::validator_for(schema).expect("compiles");
        for (sample, accepted) in samples {
            let helper = serde_json::from_value::<T>(json!({ "field": sample })).is_ok();
            assert_eq!(helper, *accepted, "helper on {sample}");
            assert_eq!(validator.is_valid(sample), *accepted, "carrier on {sample}");
        }
    }

    #[test]
    fn status_code_carrier_matches_the_helper() {
        #[derive(Deserialize)]
        struct Probe {
            #[serde(deserialize_with = "wire::deserialize_status_code")]
            #[allow(dead_code)]
            field: u16,
        }
        agree::<Probe>(
            &schema_for!(StatusCode).to_value(),
            &[
                (json!(200), true),
                (json!("404"), true),
                (json!("+200"), true),
                (json!("65535"), true),
                (json!(0), true),
                (json!("65536"), false),
                (json!(70000), false),
                (json!("abc"), false),
                (json!(-1), false),
                (json!(true), false),
            ],
        );
    }

    #[test]
    fn header_carriers_match_the_helpers() {
        #[derive(Deserialize)]
        struct Multi {
            #[serde(deserialize_with = "wire::multi_value_headers::deserialize")]
            #[allow(dead_code)]
            field: HashMap<String, Vec<String>>,
        }
        agree::<Multi>(
            &schema_for!(MultiValueHeaders).to_value(),
            &[
                (
                    json!({ "Content-Type": "text/plain", "X-N": 7, "X-B": true,
                            "Set-Cookie": ["a=1", "b=2"] }),
                    true,
                ),
                (json!({ "X": [1, true, "s"] }), true),
                (json!({ "X": { "nested": 1 } }), false),
                (json!({ "X": [["a"]] }), false),
                (json!({ "X": null }), false),
                (json!("not a map"), false),
            ],
        );

        #[derive(Deserialize)]
        struct Single {
            #[serde(deserialize_with = "wire::single_value_headers::deserialize")]
            #[allow(dead_code)]
            field: HashMap<String, String>,
        }
        agree::<Single>(
            &schema_for!(SingleValueHeaders).to_value(),
            &[
                (json!({ "X-Trace": "abc" }), true),
                (json!({ "X-Trace": 7 }), false),
                (json!({ "X-Trace": ["a"] }), false),
                (json!([]), false),
            ],
        );
    }

    #[test]
    fn one_or_many_accepts_one_many_and_null() {
        let schema = schema_for!(OneOrMany<String>).to_value();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for ok in [json!("cmd"), json!(["a", "b"]), json!(null)] {
            assert!(validator.is_valid(&ok), "{ok}");
        }
        assert!(!validator.is_valid(&json!(7)));
        assert!(!validator.is_valid(&json!([7])));
    }

    /// `U64OrNumericString` and `PemList` mirror helpers in `rift-mock-core` (`de_u64_or_string`,
    /// `ca_pem_list`), out of reach here, so these are literal shapes; the corpus and docs
    /// validation gate in `rift-http-proxy` is what runs those helpers and the schema together.
    #[test]
    fn numeric_string_and_pem_carriers() {
        let schema = schema_for!(U64OrNumericString).to_value();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&json!(50)));
        assert!(validator.is_valid(&json!("50")));
        assert!(!validator.is_valid(&json!("fifty")));
        assert!(!validator.is_valid(&json!(-5)));

        let schema = schema_for!(PemList).to_value();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&json!("-----BEGIN CERTIFICATE-----")));
        assert!(validator.is_valid(&json!(["a", "b"])));
        assert!(!validator.is_valid(&json!([1])));
    }

    #[test]
    fn selector_helpers_require_a_string_selector() {
        for schema in [jsonpath_selector(), xpath_selector()] {
            let validator = jsonschema::validator_for(&schema).unwrap();
            assert!(validator.is_valid(&json!({ "selector": "$.a" })));
            assert!(!validator.is_valid(&json!({})));
            assert!(!validator.is_valid(&json!({ "selector": 1 })));
            assert!(!validator.is_valid(&json!({ "selector": "$", "extra": 1 })));
        }
        let xpath = jsonschema::validator_for(&xpath_selector()).unwrap();
        assert!(xpath.is_valid(&json!({ "selector": "//a", "ns": { "a": "urn:x" } })));
        assert!(!xpath.is_valid(&json!({ "selector": "//a", "ns": { "a": 1 } })));
        let jsonpath = jsonschema::validator_for(&jsonpath_selector()).unwrap();
        assert!(!jsonpath.is_valid(&json!({ "selector": "$", "ns": {} })));
    }
}
