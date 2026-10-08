//! The imposter document's grammar as a JSON Schema (issue #1342).
//!
//! Generated from the config types in this crate — the raw door types where there is one, since
//! those are what `POST /imposters` and a config file accept — so it cannot say something the
//! engine does not read. The checked-in copy at `sdk-conformance/schema/imposter.schema.json` is
//! what ships in the conformance tarball; `rift-lint schema` regenerates it, and a drift test in
//! `rift-lint` fails when the two differ.
//!
//! The schema is strict where the engine is lenient: every object closes with
//! `additionalProperties: false`, except the two `#[serde(flatten)]` maps an embedder's provider
//! reads (`_rift.flowState`, `_rift.sequencing`). The engine itself refuses no unknown key —
//! rift-cluster replays stored config bytes, so a refusal at decode would make admitted configs
//! undecodable — which is exactly why a schema that does refuse them is needed: a DSL emitting a
//! key the engine drops gets told so here, not by a mock that silently behaves differently.
//!
//! Beyond standard JSON Schema, a few `x-rift-*` extensions carry the lists `rift-lint` keeps by
//! hand, so each can be tested against the schema:
//! - `x-rift-known-keys` on `predicateGenerators` items: the keys the recorder reads;
//! - `x-rift-canonical-order` on `ResponseBehaviors`: the order one object's behaviors run in;
//! - `x-rift-response-variants` on `StubResponse`: the response-type keys, in decode precedence;
//! - `x-rift-carrier: true` on a field the engine keeps and returns but never reads;
//! - `x-rift-rewrites-body: true` on a behavior that rewrites the response body.

use crate::behaviors::CANONICAL_ORDER;
use crate::imposter::{ImposterConfig, RESPONSE_VARIANT_KEYS};
use crate::recording::ProxyMode;
use rift_types::PREDICATE_GENERATOR_KEYS;
use schemars::generate::SchemaSettings;
use schemars::{Schema, SchemaGenerator, json_schema};
use serde_json::{Map, Value, json};

/// The schema's `$id`: stable across releases, so a consumer can pin by it. The engine version it
/// was generated from is in `manifest.json`, next to it in the tarball.
pub const SCHEMA_ID: &str =
    "https://github.com/achird-labs/rift/sdk-conformance/schema/imposter.schema.json";

/// The alias spellings the imposter accepts, as `(property, alias)`: each alias reads exactly as
/// the property does. Pinned to the `#[serde(alias)]` attributes by `aliases_decode_as_their_field`.
pub(crate) const IMPOSTER_ALIASES: [(&str, &str); 3] = [
    ("allowCORS", "allowCors"),
    ("serviceName", "service_name"),
    ("serviceInfo", "service_info"),
];

/// The imposter document's JSON Schema (draft 2020-12), as a JSON value. Deterministic: the same
/// engine build always produces the same value, which is what lets the checked-in copy be tested
/// for drift.
#[must_use]
pub fn imposter_schema() -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    let mut schema = generator.root_schema_for::<ImposterConfig>();
    let root = schema.ensure_object();
    root.insert("$id".to_owned(), json!(SCHEMA_ID));
    root.insert("title".to_owned(), json!("Rift imposter"));
    // Both definitions are generated from types in this crate, so their absence is a defect in
    // this module, never a runtime condition — and a generator that quietly dropped a marker would
    // ship a schema the linter's grammar tests then fail against.
    let defs = root
        .get_mut("$defs")
        .and_then(Value::as_object_mut)
        .expect("the imposter schema has $defs");
    defs.get_mut("ResponseBehaviors")
        .expect("$defs/ResponseBehaviors is generated from this crate's type")["x-rift-canonical-order"] =
        json!(CANONICAL_ORDER);
    defs.get_mut("StubResponse")
        .expect("$defs/StubResponse is generated from this crate's type")["x-rift-response-variants"] =
        json!(RESPONSE_VARIANT_KEYS);
    let mut value = schema.to_value();
    close_objects(&mut value);
    value
}

/// Whether a schema object's `key` holds a literal (`default`, an `enum` …) or an `x-rift-*`
/// extension rather than a subschema — the children a walk over schemas must not descend into.
fn is_literal_or_extension(key: &str) -> bool {
    matches!(key, "default" | "enum" | "const" | "examples") || key.starts_with("x-")
}

/// Close every object schema that lists `properties` and says nothing about other keys. A schema
/// that already decides — the flattened provider maps say `true`, the hand-written ones say
/// `false` — is left as it is.
fn close_objects(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("properties") && !map.contains_key("additionalProperties") {
                map.insert("additionalProperties".to_owned(), json!(false));
            }
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    // The values of `properties` are schemas; the map itself is not one.
                    "properties" => {
                        if let Some(properties) = child.as_object_mut() {
                            properties.values_mut().for_each(close_objects);
                        }
                    }
                    key if is_literal_or_extension(key) => {}
                    _ => close_objects(child),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(close_objects),
        _ => {}
    }
}

/// `schemars` transform for [`ImposterConfig`]: each alias in [`IMPOSTER_ALIASES`] becomes a
/// property of its own, so a document spelled the old way validates. A property the table names
/// that the struct no longer has is a defect in the table, said loudly rather than skipped.
pub(crate) fn alias_properties(schema: &mut Schema) {
    let properties = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .expect("ImposterConfig is an object schema with properties");
    for (property, alias) in IMPOSTER_ALIASES {
        let mut aliased = properties.get(property).cloned().unwrap_or_else(|| {
            panic!("IMPOSTER_ALIASES names `{property}`, not an imposter property")
        });
        aliased["description"] = json!(alias_description(property));
        properties.insert(alias.to_owned(), aliased);
    }
}

fn alias_description(property: &str) -> String {
    format!("Alias of `{property}`, read the same way.")
}

/// `proxy.mode`: the canonical spellings, or `""` for the default — the form an omitted mode is
/// written back in. The engine also reads each spelling in any case and with surrounding
/// whitespace; a document is expected to write the canonical one.
pub(crate) fn proxy_mode(_: &mut SchemaGenerator) -> Schema {
    let spellings: Vec<&str> = ProxyMode::SPELLINGS.iter().copied().chain([""]).collect();
    json_schema!({
        "description": "The recording mode: `proxyOnce` (the default, also spelled `\"\"`), \
                        `proxyAlways` or `proxyTransparent`.",
        "type": "string",
        "enum": spellings
    })
}

/// `protocol`: the two the engine serves. Anything else is refused at creation.
pub(crate) fn protocol(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "description": "The protocol the imposter serves.",
        "type": "string",
        "enum": ["http", "https"],
        "default": "http"
    })
}

/// `proxy.predicateGenerators`: each entry is read by key, so its known keys are listed both as
/// properties and as `x-rift-known-keys`, from the one constant the recorder reads.
pub(crate) fn predicate_generators(_: &mut SchemaGenerator) -> Schema {
    let mut properties = Map::new();
    // Every key is matched explicitly; a key added to the list without a shape here gets `true`,
    // which `generator_keys_have_a_deliberate_shape` refuses.
    for key in PREDICATE_GENERATOR_KEYS {
        let schema = match key {
            "inject" | "predicateOperator" | "except" => json!({ "type": "string" }),
            "caseSensitive" => json!({ "type": "boolean" }),
            "matches" | "ignore" => json!({ "type": "object" }),
            "jsonpath" => rift_types::schema::jsonpath_selector(),
            "xpath" => rift_types::schema::xpath_selector(),
            _ => Value::Bool(true),
        };
        properties.insert(key.to_owned(), schema);
    }
    json_schema!({
        "description": "How a recorded response's predicates are generated from the request \
                        that was proxied (Mountebank's `predicateGenerators`).",
        "type": "array",
        "items": {
            "type": "object",
            "properties": properties,
            "additionalProperties": false,
            "x-rift-known-keys": PREDICATE_GENERATOR_KEYS
        }
    })
}

/// `behaviors` (no underscore): one behaviors object, or an array of them. An array element may
/// be `null`, which configures nothing.
pub(crate) fn behaviors_block(generator: &mut SchemaGenerator) -> Schema {
    let block = generator.subschema_for::<crate::behaviors::ResponseBehaviors>();
    json_schema!({
        "description": "The behaviors block in array form: each element's behaviors are steps, \
                        run in array order. The object form is the same as `_behaviors`.",
        "anyOf": [
            block,
            { "type": "array", "items": { "anyOf": [block, { "type": "null" }] } },
            { "type": "null" }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::StubResponse;
    use serde_json::json;

    fn defs(schema: &Value) -> &Map<String, Value> {
        schema["$defs"].as_object().expect("$defs")
    }

    #[test]
    fn the_schema_is_a_valid_draft_2020_12_document() {
        let schema = imposter_schema();
        assert_eq!(
            schema["$schema"],
            json!("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(schema["$id"], json!(SCHEMA_ID));
        // A broken `$ref` or keyword is a compile error, not a validation failure.
        let validator = jsonschema::validator_for(&schema).expect("compiles");
        assert!(validator.is_valid(&json!({ "port": 4545, "protocol": "http", "stubs": [] })));
    }

    #[test]
    fn generation_is_deterministic() {
        assert_eq!(imposter_schema(), imposter_schema());
    }

    #[test]
    fn every_object_is_closed_except_the_provider_maps() {
        let schema = imposter_schema();
        let mut open = Vec::new();
        fn walk(value: &Value, path: &str, open: &mut Vec<String>) {
            let Some(map) = value.as_object() else {
                if let Some(items) = value.as_array() {
                    for (i, item) in items.iter().enumerate() {
                        walk(item, &format!("{path}/{i}"), open);
                    }
                }
                return;
            };
            if map.contains_key("properties")
                && map.get("additionalProperties") != Some(&json!(false))
            {
                open.push(path.to_owned());
            }
            for (key, child) in map {
                if key == "properties" {
                    for (name, property) in child.as_object().into_iter().flatten() {
                        walk(property, &format!("{path}/properties/{name}"), open);
                    }
                } else if !is_literal_or_extension(key) {
                    walk(child, &format!("{path}/{key}"), open);
                }
            }
        }
        walk(&schema, "", &mut open);
        open.sort();
        assert_eq!(
            open,
            ["/$defs/RiftFlowStateConfig", "/$defs/RiftSequencingConfig"],
            "only the two provider maps stay open"
        );
        for provider in ["RiftFlowStateConfig", "RiftSequencingConfig"] {
            assert_eq!(defs(&schema)[provider]["additionalProperties"], json!(true));
        }
    }

    #[test]
    fn the_root_is_the_imposter_and_the_doors_are_the_raw_shapes() {
        let schema = imposter_schema();
        let imposter = schema["properties"].as_object().expect("properties");
        for key in [
            "port",
            "protocol",
            "stubs",
            "_rift",
            "defaultResponse",
            "allowCORS",
        ] {
            assert!(imposter.contains_key(key), "{key}");
        }
        let stub = &defs(&schema)["Stub"]["properties"];
        for key in [
            "predicates",
            "rules",
            "responses",
            "delayRange",
            "_verify",
            "routePattern",
        ] {
            assert!(stub.get(key).is_some(), "Stub.{key}");
        }
        let response = &defs(&schema)["StubResponse"];
        for key in [
            "is",
            "proxy",
            "inject",
            "fault",
            "_behaviors",
            "behaviors",
            "_rift",
            "statusCode",
            "headers",
            "body",
            "_mode",
            "repeat",
        ] {
            assert!(
                response["properties"].get(key).is_some(),
                "StubResponse.{key}"
            );
        }
        assert_eq!(
            response["x-rift-response-variants"],
            json!(RESPONSE_VARIANT_KEYS)
        );
        assert_eq!(
            defs(&schema)["ResponseBehaviors"]["x-rift-canonical-order"],
            json!(CANONICAL_ORDER)
        );
        let generator =
            &schema["$defs"]["ProxyResponse"]["properties"]["predicateGenerators"]["items"];
        assert_eq!(
            generator["x-rift-known-keys"],
            json!(PREDICATE_GENERATOR_KEYS)
        );
        let mut known: Vec<&str> = generator["properties"]
            .as_object()
            .expect("generator properties")
            .keys()
            .map(String::as_str)
            .collect();
        known.sort_unstable();
        let mut expected = PREDICATE_GENERATOR_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(known, expected);
    }

    #[test]
    fn aliases_are_alternate_properties() {
        let schema = imposter_schema();
        let properties = schema["properties"].as_object().expect("properties");
        for (property, alias) in IMPOSTER_ALIASES {
            let mut expected = properties[property].clone();
            expected["description"] = json!(alias_description(property));
            assert_eq!(properties[alias], expected, "{alias}");
        }
    }

    /// Every generator key has a shape of its own — none fell through to the `true` a key without
    /// a shape gets.
    #[test]
    fn generator_keys_have_a_deliberate_shape() {
        let schema = imposter_schema();
        let items = &schema["$defs"]["ProxyResponse"]["properties"]["predicateGenerators"]["items"];
        for (key, property) in items["properties"].as_object().expect("properties") {
            assert!(property.is_object(), "{key} has no shape: {property}");
        }
    }

    /// Each alias in `IMPOSTER_ALIASES` decodes into the field its property names — the constant
    /// is pinned to the `#[serde(alias)]` attributes, not to memory.
    #[test]
    fn aliases_decode_as_their_field() {
        // A sample per property; an alias added to the table without one fails here.
        let samples = |property: &str| match property {
            "allowCORS" => json!(true),
            "serviceName" => json!("svc"),
            "serviceInfo" => json!({ "team": "a" }),
            other => panic!("no sample for `{other}`"),
        };
        for (property, alias) in IMPOSTER_ALIASES {
            let decode = |key: &str| -> Value {
                let config: ImposterConfig =
                    serde_json::from_value(json!({ "protocol": "http", key: samples(property) }))
                        .unwrap_or_else(|e| panic!("{key}: {e}"));
                serde_json::to_value(config).expect("serializes")
            };
            let written = decode(property);
            assert_eq!(decode(alias), written, "{alias} reads as {property}");
            assert_ne!(
                written,
                serde_json::to_value(ImposterConfig::default()).expect("serializes"),
                "the sample for {property} must change something"
            );
        }
    }

    #[test]
    fn hand_written_shapes_accept_what_the_engine_accepts() {
        let schema = imposter_schema();
        let validator = jsonschema::validator_for(&schema).expect("compiles");
        let imposter = |response: Value| json!({ "port": 4545, "protocol": "http", "stubs": [{ "responses": [response] }] });
        let predicate = |predicate: Value| json!({ "port": 4545, "protocol": "http", "stubs": [{ "predicates": [predicate] }] });
        let errors = |document: &Value| -> Vec<String> {
            validator
                .iter_errors(document)
                .map(|e| format!("{}: {e}", e.instance_path()))
                .collect()
        };
        let valid = |document: Value| assert_eq!(errors(&document), Vec::<String>::new());
        let refused = |document: Value| assert!(!errors(&document).is_empty(), "{document}");

        // Every tcp fault spelling, bare and probabilistic; an unknown kind or a probability-less
        // object form is refused.
        for kind in crate::imposter::fault_io::TcpFaultKind::SPELLINGS {
            valid(imposter(
                json!({ "is": {}, "_rift": { "fault": { "tcp": kind } } }),
            ));
            valid(imposter(json!({
                "is": {}, "_rift": { "fault": { "tcp": { "probability": 0.5, "type": kind } } }
            })));
        }
        refused(imposter(
            json!({ "is": {}, "_rift": { "fault": { "tcp": "nope" } } }),
        ));
        refused(imposter(
            json!({ "is": {}, "_rift": { "fault": { "tcp": { "type": "reset" } } } }),
        ));
        // Status code as a numeric string, multi-value headers with scalars, the flat form.
        valid(imposter(json!({
            "statusCode": "201", "headers": { "X-N": 1, "X-B": true, "Set-Cookie": ["a", "b"] }
        })));
        // `mode: ""` is what an omitted proxy mode is written back as.
        valid(imposter(
            json!({ "proxy": { "to": "http://x", "mode": "" } }),
        ));
        refused(imposter(
            json!({ "proxy": { "to": "http://x", "mode": "once" } }),
        ));
        // Behaviors: object form, array form with a null element and a null key.
        valid(imposter(json!({
            "is": {},
            "behaviors": [null, { "wait": 10, "copy": null }, { "decorate": "x" }, { "repeat": 2 }]
        })));
        valid(imposter(json!({
            "is": {},
            "_behaviors": {
                "wait": { "min": 1, "max": 2 },
                "shellTransform": ["a", "b"],
                "lookup": { "key": { "from": "path", "using": { "method": "regex", "selector": "x" } },
                            "fromDataSource": { "csv": { "path": "p", "keyColumn": "k" } },
                            "into": "${R}" }
            }
        })));
        refused(imposter(json!({ "is": {}, "_behaviors": { "waitt": 10 } })));
        // Predicates: one operator, nested, with a selector; two operators or both selectors are
        // refused.
        valid(predicate(json!({ "and": [
            { "equals": { "path": "/x" }, "caseSensitive": false },
            { "not": { "exists": { "query": { "q": true } } } },
            { "matches": { "body": "a" }, "jsonpath": { "selector": "$.a" }, "except": "z" },
            { "inject": "function (c) { return true; }" }
        ] })));
        refused(predicate(
            json!({ "equals": { "path": "/x" }, "contains": { "path": "x" } }),
        ));
        refused(predicate(json!({
            "equals": { "body": "x" }, "jsonpath": { "selector": "$" }, "xpath": { "selector": "/" }
        })));
        // Aliases, `ca` as one string, `delayRange` with numeric strings, `rules`, any `_verify`.
        valid(json!({
            "port": 4545, "protocol": "https", "allowCors": true, "service_name": "x",
            "service_info": { "team": "a" }, "cert": "c", "key": "k", "ca": "pem",
            "stubs": [{
                "delayRange": [{ "min": "5", "max": 10 }],
                "rules": [{ "equals": { "path": "/" } }],
                "_verify": { "anything": [1, "the rift-verify grammar is not this schema's"] },
                "responses": [{ "is": {} }]
            }]
        }));
        // `defaultResponse` reads `statusCode` as a number only, unlike an `is` response.
        valid(json!({ "protocol": "http", "defaultResponse": { "statusCode": 404 } }));
        let default_string =
            json!({ "protocol": "http", "defaultResponse": { "statusCode": "404" } });
        refused(default_string.clone());
        assert!(
            serde_json::from_value::<ImposterConfig>(default_string).is_err(),
            "the engine refuses it too"
        );
        // `repeat` is a u32 wherever it is read.
        valid(imposter(json!({ "is": {}, "repeat": 3 })));
        refused(imposter(json!({ "is": {}, "repeat": 5_000_000_000u64 })));
        refused(imposter(
            json!({ "is": {}, "_behaviors": { "repeat": 5_000_000_000u64 } }),
        ));
        // An unread key anywhere is refused.
        refused(json!({ "port": 1, "protocol": "http", "stubs": [], "name2": "x" }));
        refused(imposter(
            json!({ "is": { "statusCode": 200, "bodyy": "x" } }),
        ));
    }

    /// `RESPONSE_VARIANT_KEYS` lists the response-type keys in the order `TryFrom<StubResponseRaw>`
    /// prefers them: with every key present, the first one is the response.
    #[test]
    fn response_variant_keys_follow_the_decode_precedence() {
        let full = |from: usize| {
            let mut response = Map::new();
            let values = [
                json!({}),
                json!({ "to": "http://x" }),
                json!("function () {}"),
                json!("CONNECTION_RESET_BY_PEER"),
            ];
            for (key, value) in RESPONSE_VARIANT_KEYS.iter().zip(values).skip(from) {
                response.insert((*key).to_owned(), value);
            }
            serde_json::from_value::<StubResponse>(Value::Object(response)).expect("decodes")
        };
        assert!(matches!(full(0), StubResponse::Is { .. }));
        assert!(matches!(full(1), StubResponse::Proxy { .. }));
        assert!(matches!(full(2), StubResponse::Inject { .. }));
        assert!(matches!(full(3), StubResponse::Fault { .. }));
    }
}
