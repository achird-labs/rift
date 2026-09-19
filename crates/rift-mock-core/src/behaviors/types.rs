//! Configuration types for response behaviors.

use super::copy::CopyBehavior;
use super::lookup::LookupBehavior;
use super::wait::WaitBehavior;
use serde::{Deserialize, Serialize};

/// The behavior keys in the order Rift runs them when one object sets several — the object form
/// (`_behaviors`) and a multi-key element of the array form. An array's elements otherwise run in
/// array order (issue #1198).
pub const CANONICAL_ORDER: [&str; 5] = ["wait", "copy", "lookup", "decorate", "shellTransform"];

/// One behavior, as run: a response's behaviors are an ordered program of these (issue #1198).
#[derive(Debug, Clone)]
pub enum BehaviorStep {
    Wait(WaitBehavior),
    Copy(CopyBehavior),
    Lookup(LookupBehavior),
    Decorate(String),
    ShellTransform(String),
}

/// A response's behaviors as Mountebank runs them: every step, in order, plus the `repeat` the
/// response cycler reads.
#[derive(Debug, Clone, Default)]
pub struct BehaviorProgram {
    pub repeat: Option<u32>,
    pub steps: Vec<BehaviorStep>,
}

impl BehaviorProgram {
    /// Parse a program from its elements, each a behaviors object; the steps of one element run in
    /// [`CANONICAL_ORDER`], elements in the order given, and the last `repeat` set wins.
    ///
    /// # Errors
    /// The first element that is not a valid behaviors object.
    pub fn from_elements<'a>(
        elements: impl IntoIterator<Item = &'a serde_json::Value>,
    ) -> Result<Self, serde_json::Error> {
        let mut program = Self::default();
        for element in elements {
            let behaviors = ResponseBehaviors::deserialize(element)?;
            if behaviors.repeat.is_some() {
                program.repeat = behaviors.repeat;
            }
            program.steps.extend(behaviors.into_steps());
        }
        Ok(program)
    }

    /// Whether any step acts on the response itself. `repeat` alone does not: the response cycler
    /// reads it, and nothing is run on the response.
    #[must_use]
    pub fn transforms_response(&self) -> bool {
        !self.steps.is_empty()
    }
}

/// Response behaviors that modify how responses are generated
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResponseBehaviors {
    /// Add latency before response
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitBehavior>,

    /// Repeat response N times before advancing to next
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,

    /// Copy fields from request to response
    /// Mountebank allows both single object and array format
    #[serde(
        default,
        deserialize_with = "deserialize_copy_behaviors",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub copy: Vec<CopyBehavior>,

    /// Lookup from external data source
    #[serde(
        default,
        deserialize_with = "deserialize_lookup_behaviors",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub lookup: Vec<LookupBehavior>,

    /// Shell transform - external program(s) transform response.
    /// Accepts a single command string or an array of commands chained in sequence.
    /// Each program receives MB_REQUEST and MB_RESPONSE env vars; stdout becomes the next response.
    #[serde(
        default,
        deserialize_with = "deserialize_shell_transforms",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub shell_transform: Vec<String>,

    /// Decorate - Rhai script to post-process response (Mountebank-compatible)
    /// Script receives `request` and `response` variables and can modify response
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decorate: Option<String>,
}

impl ResponseBehaviors {
    /// This object's behaviors as steps, in [`CANONICAL_ORDER`]. `repeat` is not a step.
    #[must_use]
    pub fn into_steps(self) -> Vec<BehaviorStep> {
        let mut steps: Vec<BehaviorStep> = self.wait.into_iter().map(BehaviorStep::Wait).collect();
        steps.extend(self.copy.into_iter().map(BehaviorStep::Copy));
        steps.extend(self.lookup.into_iter().map(BehaviorStep::Lookup));
        steps.extend(self.decorate.into_iter().map(BehaviorStep::Decorate));
        steps.extend(
            self.shell_transform
                .into_iter()
                .map(BehaviorStep::ShellTransform),
        );
        steps
    }
}

/// Deserialize shellTransform accepting a single string or an array of strings.
fn deserialize_shell_transforms<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct ShellTransformVisitor;

    impl<'de> Visitor<'de> for ShellTransformVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a shell command string or array of shell command strings")
        }

        /// An explicit `null` is the key absent (issue #1093), as it already is for the `Option`
        /// behaviors; without this the whole block failed to parse and every behavior was dropped.
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_string()])
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(vec![v])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut commands = Vec::new();
            while let Some(cmd) = seq.next_element::<String>()? {
                commands.push(cmd);
            }
            Ok(commands)
        }
    }

    deserializer.deserialize_any(ShellTransformVisitor)
}

/// Custom deserializer for copy behaviors that accepts both object and array
fn deserialize_copy_behaviors<'de, D>(deserializer: D) -> Result<Vec<CopyBehavior>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct CopyBehaviorsVisitor;

    impl<'de> Visitor<'de> for CopyBehaviorsVisitor {
        type Value = Vec<CopyBehavior>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a copy behavior object or array of copy behaviors")
        }

        /// An explicit `null` is the key absent (issue #1093), as it already is for the `Option`
        /// behaviors; without this the whole block failed to parse and every behavior was dropped.
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut behaviors = Vec::new();
            while let Some(behavior) = seq.next_element()? {
                behaviors.push(behavior);
            }
            Ok(behaviors)
        }

        fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            // Single object - wrap in vec
            let behavior = CopyBehavior::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![behavior])
        }
    }

    deserializer.deserialize_any(CopyBehaviorsVisitor)
}

/// Accept either a single `lookup` object or an array, mirroring `copy`.
/// Mountebank and the docs use the single-object form (`"lookup": { ... }`).
fn deserialize_lookup_behaviors<'de, D>(deserializer: D) -> Result<Vec<LookupBehavior>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct LookupBehaviorsVisitor;

    impl<'de> Visitor<'de> for LookupBehaviorsVisitor {
        type Value = Vec<LookupBehavior>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a lookup behavior object or array of lookup behaviors")
        }

        /// An explicit `null` is the key absent (issue #1093), as it already is for the `Option`
        /// behaviors; without this the whole block failed to parse and every behavior was dropped.
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut behaviors = Vec::new();
            while let Some(behavior) = seq.next_element()? {
                behaviors.push(behavior);
            }
            Ok(behaviors)
        }

        fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let behavior = LookupBehavior::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![behavior])
        }
    }

    deserializer.deserialize_any(LookupBehaviorsVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_behaviors_serde() {
        let yaml = r#"
wait: 500
repeat: 3
copy:
  - from: path
    into: "${PATH}"
    using:
      method: regex
      selector: ".*"
"#;
        let behaviors: ResponseBehaviors = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(behaviors.wait, Some(WaitBehavior::Fixed(500))));
        assert_eq!(behaviors.repeat, Some(3));
        assert_eq!(behaviors.copy.len(), 1);
    }

    // The `lookup` field accepts both a single object (Mountebank/docs form) and
    // an array, mirroring `copy`.
    #[test]
    fn test_lookup_behaviors_single_object_and_array() {
        let lookup = serde_json::json!({
            "key": { "from": "path", "using": { "method": "regex", "selector": "/c/(\\d+)" } },
            "fromDataSource": { "csv": { "path": "x.csv", "keyColumn": "id" } },
            "into": "${row}"
        });

        let single: ResponseBehaviors =
            serde_json::from_value(serde_json::json!({ "lookup": lookup.clone() })).unwrap();
        assert_eq!(
            single.lookup.len(),
            1,
            "single object should yield one behavior"
        );

        let array: ResponseBehaviors =
            serde_json::from_value(serde_json::json!({ "lookup": [lookup.clone(), lookup] }))
                .unwrap();
        assert_eq!(array.lookup.len(), 2, "array should yield two behaviors");
    }

    // Issue #1088 pins this: rift-lint's E025 accepts a bare `wait` only when it is a non-negative
    // integer, because anything else parses into no variant and the parsed block is ignored.
    #[test]
    fn a_fractional_or_negative_wait_does_not_parse() {
        for wait in [
            serde_json::json!(500.5),
            serde_json::json!(500.0),
            serde_json::json!(-1),
        ] {
            let err =
                serde_json::from_value::<ResponseBehaviors>(serde_json::json!({ "wait": wait }))
                    .expect_err("a non-u64 wait must not parse");
            assert!(
                err.to_string().contains("untagged enum WaitBehavior"),
                "wait {wait}: {err}"
            );
        }
        let zero: ResponseBehaviors =
            serde_json::from_value(serde_json::json!({ "wait": 0 })).unwrap();
        assert!(matches!(zero.wait, Some(WaitBehavior::Fixed(0))));
    }

    /// Issue #1093: an explicit `null` for any behavior key is that key absent. `wait`, `repeat`
    /// and `decorate` got this from `Option`; the three list keys used to fail the whole block.
    #[test]
    fn an_explicit_null_behavior_key_is_absent() {
        let json: ResponseBehaviors = serde_json::from_value(serde_json::json!({
            "wait": null, "repeat": null, "decorate": null,
            "shellTransform": null, "copy": null, "lookup": null
        }))
        .expect("every null key parses");
        let yaml: ResponseBehaviors = serde_yaml::from_str(
            "wait: ~\nrepeat: ~\ndecorate: ~\nshellTransform: ~\ncopy: ~\nlookup: ~\n",
        )
        .expect("every null key parses from YAML");
        for parsed in [json, yaml] {
            assert!(parsed.wait.is_none());
            assert_eq!(parsed.repeat, None);
            assert_eq!(parsed.decorate, None);
            assert!(parsed.shell_transform.is_empty());
            assert!(parsed.copy.is_empty());
            assert!(parsed.lookup.is_empty());
        }

        // The original failure: a null list key dropped the live behaviors beside it.
        let siblings: ResponseBehaviors = serde_json::from_value(serde_json::json!({
            "shellTransform": null, "copy": null, "lookup": null,
            "wait": 5, "repeat": 2, "decorate": "response.body = 'x';"
        }))
        .expect("null list keys leave their siblings parseable");
        assert!(matches!(siblings.wait, Some(WaitBehavior::Fixed(5))));
        assert_eq!(siblings.repeat, Some(2));
        assert_eq!(siblings.decorate.as_deref(), Some("response.body = 'x';"));
    }

    #[test]
    fn test_shell_transform_config_serde() {
        let yaml = r#"
wait: 100
shellTransform: "echo 'transformed'"
"#;
        let behaviors: ResponseBehaviors = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(behaviors.wait, Some(WaitBehavior::Fixed(100))));
        assert_eq!(behaviors.shell_transform, vec!["echo 'transformed'"]);
    }

    #[test]
    fn test_shell_transform_array_serde() {
        let yaml = r#"
shellTransform:
  - "./transform1.sh"
  - "./transform2.sh"
"#;
        let behaviors: ResponseBehaviors = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            behaviors.shell_transform,
            vec!["./transform1.sh", "./transform2.sh"]
        );
    }

    #[test]
    fn test_decorate_behavior_serde() {
        let yaml = r#"
wait: 100
decorate: "response.body = 'decorated';"
"#;
        let behaviors: ResponseBehaviors = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(behaviors.wait, Some(WaitBehavior::Fixed(100))));
        assert_eq!(
            behaviors.decorate,
            Some("response.body = 'decorated';".to_string())
        );
    }
}
