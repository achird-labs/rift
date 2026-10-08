//! Every grammar list this linter keeps by hand equals the set the generated imposter schema
//! carries for it (issue #1342). The schema is read from the checked-in copy, which the drift test
//! in `rift-lint`'s `schema` module (under the `schema` feature) keeps equal to what the engine
//! generates — so these run in every build, and a list that drifts from the engine fails here.

use crate::validator::{
    BODY_REWRITING_BEHAVIORS, CARRIER_FIELDS, EXTRACTION_METHODS, NESTING_OPERATORS, PROTOCOLS,
    PROXY_MODES, REGEX_FLAGS, REQUIRED_FIELDS, RESPONSE_SHAPES, STEP_BEHAVIORS, TCP_FAULT_KINDS,
    UNAPPLIED_RIFT_SHAPES,
};
use rift_types::{PREDICATE_GENERATOR_KEYS, PREDICATE_OPERATORS, PREDICATE_PARAMETERS};
use serde_json::Value;
use std::path::Path;
use std::sync::LazyLock;

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(crate::CHECKED_IN_SCHEMA);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("the checked-in schema is JSON")
});

fn def(name: &str) -> &'static Value {
    let def = &SCHEMA["$defs"][name];
    assert!(def.is_object(), "no $defs/{name} in the schema");
    def
}

/// The property names of an object schema, sorted.
fn properties(schema: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("no properties in {schema}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

fn sorted<'a>(list: &[&'a str]) -> Vec<&'a str> {
    let mut list = list.to_vec();
    list.sort_unstable();
    list
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {value}"))
        .iter()
        .map(|v| v.as_str().expect("a string"))
        .collect()
}

/// The properties anywhere under `$defs` and the root that carry the `marker` extension, sorted.
fn marked(marker: &str) -> Vec<&'static str> {
    fn walk<'a>(schema: &'a Value, marker: &str, out: &mut Vec<&'a str>) {
        if let Some(properties) = schema["properties"].as_object() {
            for (name, property) in properties {
                if property.get(marker) == Some(&Value::Bool(true)) {
                    out.push(name);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&SCHEMA, marker, &mut out);
    for def in SCHEMA["$defs"].as_object().expect("$defs").values() {
        walk(def, marker, &mut out);
    }
    out.sort_unstable();
    out
}

/// The schema's predicate operators: the `oneOf` branches, each requiring exactly one.
fn schema_operators() -> Vec<&'static str> {
    let mut operators: Vec<&str> = def("Predicate")["oneOf"]
        .as_array()
        .expect("Predicate.oneOf")
        .iter()
        .map(|branch| {
            let required = strings(&branch["required"]);
            assert_eq!(required.len(), 1, "{branch}");
            required[0]
        })
        .collect();
    operators.sort_unstable();
    operators
}

#[test]
fn predicate_operators_are_the_schemas_one_of_branches() {
    assert_eq!(schema_operators(), sorted(&PREDICATE_OPERATORS));
}

#[test]
fn predicate_parameters_are_the_schemas_other_properties() {
    let operators = schema_operators();
    let parameters: Vec<&str> = properties(def("Predicate"))
        .into_iter()
        .filter(|key| !operators.contains(key))
        .collect();
    assert_eq!(parameters, sorted(&PREDICATE_PARAMETERS));
}

#[test]
fn predicate_generator_keys_are_the_schemas_known_keys() {
    let items = &def("ProxyResponse")["properties"]["predicateGenerators"]["items"];
    assert_eq!(
        strings(&items["x-rift-known-keys"]),
        PREDICATE_GENERATOR_KEYS
    );
    assert_eq!(properties(items), sorted(&PREDICATE_GENERATOR_KEYS));
    assert_eq!(items["additionalProperties"], Value::Bool(false));
}

#[test]
fn step_behaviors_are_the_schemas_canonical_order() {
    let behaviors = def("ResponseBehaviors");
    assert_eq!(
        strings(&behaviors["x-rift-canonical-order"]),
        STEP_BEHAVIORS
    );
    let steps: Vec<&str> = properties(behaviors)
        .into_iter()
        .filter(|key| *key != "repeat")
        .collect();
    assert_eq!(steps, sorted(&STEP_BEHAVIORS));
}

#[test]
fn tcp_fault_kinds_are_the_schemas_enum() {
    let fault = def("RiftTcpFault");
    let bare = strings(&fault["anyOf"][0]["enum"]);
    assert_eq!(bare, TCP_FAULT_KINDS);
    let typed = strings(&fault["anyOf"][1]["properties"]["type"]["enum"]);
    assert_eq!(typed, TCP_FAULT_KINDS);
}

#[test]
fn unapplied_rift_shapes_are_the_response_variants_but_is() {
    let variants = strings(&def("StubResponse")["x-rift-response-variants"]);
    assert_eq!(variants[0], "is");
    assert_eq!(variants[1..], UNAPPLIED_RIFT_SHAPES);
}

#[test]
fn response_shapes_are_the_response_variants_but_is_plus_rift() {
    let variants = strings(&def("StubResponse")["x-rift-response-variants"]);
    let mut shapes: Vec<&str> = variants[1..].to_vec();
    shapes.push("_rift");
    assert_eq!(shapes, RESPONSE_SHAPES);
    assert!(def("StubResponse")["properties"].get("_rift").is_some());
}

#[test]
fn carrier_fields_are_the_schemas_carriers() {
    assert_eq!(marked("x-rift-carrier"), sorted(&CARRIER_FIELDS));
}

#[test]
fn body_rewriting_behaviors_are_the_schemas_rewriters() {
    assert_eq!(
        marked("x-rift-rewrites-body"),
        sorted(&BODY_REWRITING_BEHAVIORS)
    );
}

#[test]
fn required_fields_are_imposter_properties_the_engine_does_not_require() {
    let root = properties(&SCHEMA);
    for field in REQUIRED_FIELDS {
        assert!(root.contains(&field), "{field} is not an imposter property");
    }
    // The engine requires nothing; E003 is the linter's own policy, which is why this is not an
    // equality with the schema's `required`.
    assert!(SCHEMA.get("required").is_none());
}

#[test]
fn protocols_are_the_schemas_enum_plus_tcp() {
    let served = strings(&SCHEMA["properties"]["protocol"]["enum"]);
    let mut recognized = sorted(&PROTOCOLS);
    recognized.retain(|p| *p != "tcp");
    assert_eq!(sorted(&served), recognized);
    assert!(
        PROTOCOLS.contains(&"tcp"),
        "tcp is recognized only to be refused"
    );
}

#[test]
fn nesting_operators_are_the_ones_whose_value_is_a_predicate() {
    let properties = def("Predicate")["properties"]
        .as_object()
        .expect("properties");
    let predicate_ref = Value::String("#/$defs/Predicate".to_owned());
    let mut nesting: Vec<&str> = properties
        .iter()
        .filter(|(_, schema)| {
            schema.get("$ref") == Some(&predicate_ref)
                || schema["items"].get("$ref") == Some(&predicate_ref)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    nesting.sort_unstable();
    assert_eq!(nesting, sorted(&NESTING_OPERATORS));
}

#[test]
fn proxy_modes_are_the_schemas_enum_but_the_empty_default() {
    let modes: Vec<&str> = strings(&def("ProxyResponse")["properties"]["mode"]["enum"])
        .into_iter()
        .filter(|mode| !mode.is_empty())
        .collect();
    assert_eq!(modes, PROXY_MODES);
}

#[test]
fn extraction_methods_and_regex_flags_are_the_schemas() {
    let mut methods: Vec<&str> = def("ExtractionMethod")["oneOf"]
        .as_array()
        .expect("oneOf")
        .iter()
        .map(|branch| {
            branch["properties"]["method"]["const"]
                .as_str()
                .expect("const")
        })
        .collect();
    methods.sort_unstable();
    assert_eq!(methods, sorted(&EXTRACTION_METHODS));
    assert_eq!(properties(def("RegexOptions")), sorted(&REGEX_FLAGS));
}
