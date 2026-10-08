//! Predicate types for matching requests against stubs.
//!
//! These are pure data types (no matching logic) so they can be shared across the
//! workspace — the proxy for matching, the linter for concrete-type validation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single predicate: matcher parameters plus the operation to apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Predicate {
    #[serde(flatten)]
    pub parameters: PredicateParameters,
    #[serde(flatten)]
    pub operation: PredicateOperation,
}

/// The matching operation a predicate performs (Mountebank-compatible).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PredicateOperation {
    Equals(HashMap<String, serde_json::Value>),
    DeepEquals(HashMap<String, serde_json::Value>),
    Contains(HashMap<String, serde_json::Value>),
    StartsWith(HashMap<String, serde_json::Value>),
    EndsWith(HashMap<String, serde_json::Value>),
    Matches(HashMap<String, serde_json::Value>),
    Exists(HashMap<String, serde_json::Value>),
    Not(Box<Predicate>),
    Or(Vec<Predicate>),
    And(Vec<Predicate>),
    Inject(String),
}

/// The wire names of [`PredicateOperation`]'s variants, one per variant, in declaration order.
///
/// The grammar's single list of operators (issue #1342): the JSON Schema, `rift-lint` and the
/// unread-key gate read it from here. Pinned to the enum by `operators_name_every_variant`.
pub const PREDICATE_OPERATORS: [&str; 11] = [
    "equals",
    "deepEquals",
    "contains",
    "startsWith",
    "endsWith",
    "matches",
    "exists",
    "not",
    "or",
    "and",
    "inject",
];

/// The wire names of [`PredicateParameters`]' fields, the selector's two spellings included.
/// Pinned to the struct by `parameters_name_every_field`.
pub const PREDICATE_PARAMETERS: [&str; 5] = [
    "caseSensitive",
    "keyCaseSensitive",
    "except",
    "jsonpath",
    "xpath",
];

/// The keys of a proxy `predicateGenerators` entry the engine reads (issue #1327). Any other key
/// is accepted and reported as unread, never refused: Mountebank configs carry keys Rift may not
/// implement yet. Lives here so the recorder, `rift-lint` and the schema share one list (#1342).
pub const PREDICATE_GENERATOR_KEYS: [&str; 8] = [
    "inject",
    "matches",
    "caseSensitive",
    "predicateOperator",
    "except",
    "jsonpath",
    "xpath",
    "ignore",
];

/// Matcher parameters shared across operations (case sensitivity, selectors, etc.).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PredicateParameters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_sensitive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_case_sensitive: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub except: String,
    #[serde(flatten)]
    pub selector: Option<PredicateSelector>,
}

/// A structured selector applied to the request body before matching.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PredicateSelector {
    XPath {
        selector: String,
        #[serde(rename = "ns", default, skip_serializing_if = "Option::is_none")]
        namespaces: Option<HashMap<String, String>>,
    },
    JsonPath {
        selector: String,
    },
}

/// The JSON Schema of [`Predicate`], hand-written (issue #1342): its three `#[serde(flatten)]`s
/// defeat derivation, and the shape the engine reads — exactly one operator beside the parameters —
/// is a `oneOf` over [`PREDICATE_OPERATORS`] that a derive could not express.
#[cfg(feature = "schema")]
mod schema {
    use super::{PREDICATE_OPERATORS, PREDICATE_PARAMETERS, Predicate};
    use crate::schema::{jsonpath_selector, xpath_selector};
    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
    use serde_json::{Map, Value, json};
    use std::borrow::Cow;

    impl JsonSchema for Predicate {
        fn schema_name() -> Cow<'static, str> {
            "Predicate".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            let predicate = generator.subschema_for::<Self>().to_value();
            let fields = json!({"type": "object", "additionalProperties": true});
            let mut properties = Map::new();
            // Every name is matched explicitly; a name added to the list without a shape here
            // gets `true`, which `every_predicate_property_has_a_deliberate_shape` refuses.
            for operator in PREDICATE_OPERATORS {
                let schema = match operator {
                    "equals" | "deepEquals" | "contains" | "startsWith" | "endsWith"
                    | "matches" | "exists" => fields.clone(),
                    "not" => predicate.clone(),
                    "and" | "or" => json!({"type": "array", "items": predicate}),
                    "inject" => json!({"type": "string"}),
                    _ => Value::Bool(true),
                };
                properties.insert(operator.to_owned(), schema);
            }
            for parameter in PREDICATE_PARAMETERS {
                let schema = match parameter {
                    "caseSensitive" | "keyCaseSensitive" => json!({"type": "boolean"}),
                    "except" => json!({"type": "string"}),
                    "jsonpath" => jsonpath_selector(),
                    "xpath" => xpath_selector(),
                    _ => Value::Bool(true),
                };
                properties.insert(parameter.to_owned(), schema);
            }
            let one_operator: Vec<Value> = PREDICATE_OPERATORS
                .iter()
                .map(|operator| json!({"required": [operator]}))
                .collect();
            json_schema!({
                "type": "object",
                "description": "A request predicate: exactly one operator, plus the matcher \
                                parameters it applies with. `jsonpath` and `xpath` are the two \
                                spellings of one selector, so at most one is given.",
                "properties": properties,
                "oneOf": one_operator,
                "not": {"required": ["jsonpath", "xpath"]},
                "additionalProperties": false
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every variant of `PredicateOperation` is named in `PREDICATE_OPERATORS`, under the key
    /// serde writes for it — so the list is pinned to the enum, not to anyone's memory of it.
    /// The wildcard-free `match` makes a twelfth variant a compile error here.
    #[test]
    fn operators_name_every_variant() {
        let fields = HashMap::from([("path".to_owned(), json!("/x"))]);
        let leaf = Predicate {
            parameters: PredicateParameters::default(),
            operation: PredicateOperation::Equals(fields.clone()),
        };
        match &leaf.operation {
            PredicateOperation::Equals(_)
            | PredicateOperation::DeepEquals(_)
            | PredicateOperation::Contains(_)
            | PredicateOperation::StartsWith(_)
            | PredicateOperation::EndsWith(_)
            | PredicateOperation::Matches(_)
            | PredicateOperation::Exists(_)
            | PredicateOperation::Not(_)
            | PredicateOperation::Or(_)
            | PredicateOperation::And(_)
            | PredicateOperation::Inject(_) => {}
        }
        let variants = [
            PredicateOperation::Equals(fields.clone()),
            PredicateOperation::DeepEquals(fields.clone()),
            PredicateOperation::Contains(fields.clone()),
            PredicateOperation::StartsWith(fields.clone()),
            PredicateOperation::EndsWith(fields.clone()),
            PredicateOperation::Matches(fields.clone()),
            PredicateOperation::Exists(fields),
            PredicateOperation::Not(Box::new(leaf.clone())),
            PredicateOperation::Or(vec![leaf.clone()]),
            PredicateOperation::And(vec![leaf]),
            PredicateOperation::Inject("function () {}".to_owned()),
        ];
        let written: Vec<String> = variants
            .iter()
            .map(|operation| {
                let value = serde_json::to_value(operation).unwrap();
                let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
                assert_eq!(keys.len(), 1, "{value}");
                keys.remove(0)
            })
            .collect();
        assert_eq!(written, PREDICATE_OPERATORS);
    }

    /// Every field of `PredicateParameters` — and both selector spellings — is named in
    /// `PREDICATE_PARAMETERS`, under the key serde writes for it. The `..`-free destructuring
    /// makes a new field a compile error here.
    #[test]
    fn parameters_name_every_field() {
        let with = |selector| PredicateParameters {
            case_sensitive: Some(true),
            key_case_sensitive: Some(true),
            except: "^x".to_owned(),
            selector: Some(selector),
        };
        let PredicateParameters {
            case_sensitive: _,
            key_case_sensitive: _,
            except: _,
            selector: _,
        } = PredicateParameters::default();
        match (PredicateSelector::JsonPath {
            selector: String::new(),
        }) {
            PredicateSelector::XPath { .. } | PredicateSelector::JsonPath { .. } => {}
        }
        let mut written: Vec<String> = Vec::new();
        for parameters in [
            with(PredicateSelector::JsonPath {
                selector: "$.a".to_owned(),
            }),
            with(PredicateSelector::XPath {
                selector: "//a".to_owned(),
                namespaces: None,
            }),
        ] {
            let value = serde_json::to_value(&parameters).unwrap();
            written.extend(value.as_object().unwrap().keys().cloned());
        }
        written.sort();
        written.dedup();
        let mut expected: Vec<&str> = PREDICATE_PARAMETERS.to_vec();
        expected.sort_unstable();
        assert_eq!(written, expected);
    }

    /// Every operator and parameter has a shape of its own in the hand-written schema — none fell
    /// through to the `true` a name without a shape gets.
    #[cfg(feature = "schema")]
    #[test]
    fn every_predicate_property_has_a_deliberate_shape() {
        let schema = schemars::schema_for!(Predicate).to_value();
        let properties = schema["properties"].as_object().expect("properties");
        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        let mut expected: Vec<&str> = PREDICATE_OPERATORS
            .iter()
            .chain(PREDICATE_PARAMETERS.iter())
            .copied()
            .collect();
        expected.sort_unstable();
        assert_eq!(names, expected);
        for (name, property) in properties {
            assert!(property.is_object(), "{name} has no shape: {property}");
        }
        // The schema refuses what the engine never reads: no operator, a mistyped one, a
        // mistyped parameter.
        let validator = jsonschema::validator_for(&schema).expect("compiles");
        for bad in [
            json!({ "caseSensitive": true }),
            json!({ "equals": "x" }),
            json!({ "and": {} }),
            json!({ "inject": 1 }),
            json!({ "equals": { "path": "/" }, "caseSensitive": "x" }),
        ] {
            assert!(!validator.is_valid(&bad), "{bad}");
        }
    }

    #[test]
    fn generator_keys_are_the_documented_eight() {
        // A literal pin (issue #1327): the recorder, the linter and the schema all read this list.
        assert_eq!(
            PREDICATE_GENERATOR_KEYS,
            [
                "inject",
                "matches",
                "caseSensitive",
                "predicateOperator",
                "except",
                "jsonpath",
                "xpath",
                "ignore",
            ]
        );
    }

    #[test]
    fn deserializes_into_concrete_operation() {
        // The headline benefit for the linter: match on a typed operation instead of
        // poking at a serde_json::Value.
        let pred: Predicate =
            serde_json::from_value(json!({ "equals": { "path": "/hello" } })).unwrap();
        match pred.operation {
            PredicateOperation::Equals(fields) => {
                assert_eq!(fields.get("path").unwrap(), "/hello");
            }
            other => panic!("expected Equals, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_parameters_and_selector() {
        let value = json!({
            "equals": { "body": "x" },
            "caseSensitive": false,
            "jsonpath": { "selector": "$.id" }
        });
        let pred: Predicate = serde_json::from_value(value).unwrap();
        assert_eq!(pred.parameters.case_sensitive, Some(false));
        assert!(matches!(
            pred.parameters.selector,
            Some(PredicateSelector::JsonPath { .. })
        ));
        assert!(matches!(pred.operation, PredicateOperation::Equals(_)));

        // re-serializing keeps the camelCase wire shape
        let back = serde_json::to_value(&pred).unwrap();
        assert_eq!(back["caseSensitive"], json!(false));
        assert!(back["jsonpath"]["selector"] == json!("$.id"));
    }

    #[test]
    fn xpath_selector_round_trips_with_namespaces() {
        // The XPath selector carries the most fragile serde attributes: the enum's
        // `rename_all = "lowercase"` ("xpath") and `rename = "ns"` on namespaces.
        let value = json!({
            "equals": { "body": "x" },
            "xpath": { "selector": "//a:user", "ns": { "a": "urn:y" } }
        });
        let pred: Predicate = serde_json::from_value(value).unwrap();
        let Some(PredicateSelector::XPath {
            selector,
            namespaces,
        }) = &pred.parameters.selector
        else {
            panic!("expected an XPath selector");
        };
        assert_eq!(selector, "//a:user");
        assert_eq!(namespaces.as_ref().unwrap().get("a").unwrap(), "urn:y");

        let back = serde_json::to_value(&pred).unwrap();
        assert_eq!(back["xpath"]["selector"], json!("//a:user"));
        assert_eq!(
            back["xpath"]["ns"]["a"],
            json!("urn:y"),
            "the `ns` rename is preserved"
        );
    }

    #[test]
    fn except_and_key_case_sensitive_round_trip() {
        let pred: Predicate = serde_json::from_value(json!({
            "matches": { "path": "/x" },
            "except": "^/skip",
            "keyCaseSensitive": true
        }))
        .unwrap();
        assert_eq!(pred.parameters.except, "^/skip");
        assert_eq!(pred.parameters.key_case_sensitive, Some(true));
        let back = serde_json::to_value(&pred).unwrap();
        assert_eq!(back["except"], json!("^/skip"));
        assert_eq!(back["keyCaseSensitive"], json!(true));

        // empty `except` is omitted from the wire form (skip_serializing_if)
        let empty: Predicate =
            serde_json::from_value(json!({ "matches": { "path": "/x" } })).unwrap();
        assert!(
            serde_json::to_value(&empty)
                .unwrap()
                .get("except")
                .is_none()
        );
    }

    #[test]
    fn nests_logical_operators() {
        let pred: Predicate = serde_json::from_value(json!({
            "and": [
                { "equals": { "method": "GET" } },
                { "not": { "exists": { "headers": { "X": true } } } }
            ]
        }))
        .unwrap();
        let PredicateOperation::And(subs) = pred.operation else {
            panic!("expected And");
        };
        assert_eq!(subs.len(), 2);
        assert!(matches!(subs[1].operation, PredicateOperation::Not(_)));
    }
}
