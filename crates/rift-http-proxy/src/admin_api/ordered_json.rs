//! [`OrderedJson`]: a JSON tree whose objects keep their key order (issue #1696).

use indexmap::IndexMap;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, Serializer};
use std::fmt;

/// A JSON value whose objects keep the order their keys were written or serialized in.
///
/// `serde_json::Value` sorts every object, because `preserve_order` stays off workspace-wide:
/// `stub_key` (rift-cluster's cross-node cursor key) and the proxy claim hashes are computed over
/// sorted `Value`s, so the feature cannot be turned on. A document that is decoded or re-encoded on
/// its way out — the `?replayable=true` export `rift save` writes, the C-ABI views — goes through
/// this type instead, so a stub's `is.headers` prints in the order it was written, as Mountebank
/// prints it.
///
/// It is otherwise `Value`: numbers are `serde_json::Number` (so they print the same digits), and it
/// serializes through any serializer, the admin API's pretty printer included.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderedJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OrderedJson>),
    Object(IndexMap<String, OrderedJson>),
}

impl OrderedJson {
    /// `value` as `serde_json` serializes it, every object in its serialized key order.
    ///
    /// # Errors
    /// `value`'s own serialization failure.
    pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Self, serde_json::Error> {
        serde_json::from_str(&serde_json::to_string(value)?)
    }
}

impl Serialize for OrderedJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(b) => serializer.serialize_bool(*b),
            Self::Number(n) => n.serialize(serializer),
            Self::String(s) => serializer.serialize_str(s),
            Self::Array(items) => serializer.collect_seq(items),
            Self::Object(entries) => serializer.collect_map(entries),
        }
    }
}

impl<'de> Deserialize<'de> for OrderedJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(OrderedJsonVisitor)
    }
}

struct OrderedJsonVisitor;

impl<'de> Visitor<'de> for OrderedJsonVisitor {
    type Value = OrderedJson;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(OrderedJson::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(OrderedJson::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(OrderedJson::Number(v.into()))
    }

    // As `Value` decodes it: a non-finite float (never produced by a JSON parser) is `null`.
    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        Ok(serde_json::Number::from_f64(v).map_or(OrderedJson::Null, OrderedJson::Number))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(OrderedJson::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(OrderedJson::String(v))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(OrderedJson::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(OrderedJson::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        Deserialize::deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(OrderedJson::Array(items))
    }

    // A repeated key keeps its first position and its last value, as `JSON.parse` keeps it.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = IndexMap::with_capacity(map.size_hint().unwrap_or(0).min(4096));
        while let Some((key, value)) = map.next_entry::<String, OrderedJson>()? {
            entries.insert(key, value);
        }
        Ok(OrderedJson::Object(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_keep_their_key_order_through_a_round_trip() {
        let text = r#"{"z":1,"a":{"Zeta":"1","Alpha":"2","Mid":"3"},"m":[{"y":null,"b":true}]}"#;
        let tree: OrderedJson = serde_json::from_str(text).expect("parses");
        assert_eq!(serde_json::to_string(&tree).expect("encodes"), text);
    }

    #[test]
    fn numbers_print_the_digits_value_prints() {
        let text = "[0,-1,18446744073709551615,-9223372036854775808,1.5,7e23,1e-7,-0.0]";
        let tree: OrderedJson = serde_json::from_str(text).expect("parses");
        let value: serde_json::Value = serde_json::from_str(text).expect("parses");
        assert_eq!(
            serde_json::to_string(&tree).expect("encodes"),
            serde_json::to_string(&value).expect("encodes")
        );
    }

    #[test]
    fn pretty_printing_matches_value_for_a_sorted_document() {
        // The admin API pretty-prints; an already-sorted document prints exactly as `Value` does.
        let text = r#"{"a":[],"b":{},"c":[1,{"d":"e"}],"f":null}"#;
        let tree: OrderedJson = serde_json::from_str(text).expect("parses");
        let value: serde_json::Value = serde_json::from_str(text).expect("parses");
        assert_eq!(
            serde_json::to_string_pretty(&tree).expect("encodes"),
            serde_json::to_string_pretty(&value).expect("encodes")
        );
    }

    #[test]
    fn a_repeated_key_keeps_its_first_position_and_last_value() {
        let tree: OrderedJson = serde_json::from_str(r#"{"b":1,"a":2,"b":3}"#).expect("parses");
        assert_eq!(
            serde_json::to_string(&tree).expect("encodes"),
            r#"{"b":3,"a":2}"#
        );
    }

    #[test]
    fn encode_keeps_a_structs_field_order() {
        #[derive(serde::Serialize)]
        struct Doc {
            zeta: u8,
            alpha: u8,
        }
        let tree = OrderedJson::encode(&Doc { zeta: 1, alpha: 2 }).expect("encodes");
        assert_eq!(
            serde_json::to_string(&tree).expect("encodes"),
            r#"{"zeta":1,"alpha":2}"#
        );
    }
}
