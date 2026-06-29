//! Configuration types for response behaviors.

use super::copy::CopyBehavior;
use super::lookup::LookupBehavior;
use super::wait::WaitBehavior;
use serde::{Deserialize, Serialize};

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
