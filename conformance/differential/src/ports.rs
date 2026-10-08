//! Logical ↔ actual port mapping.
//!
//! Cases are written with *logical* ports (the `4545` of the retired suite, the `450x` of the SDK
//! corpus). Both engines run side by side on one host, so each gets its own free *actual* port per
//! logical one. Everything sent to an engine is mapped forward; everything read back is mapped in
//! reverse before it is compared, so a port never shows up as a difference.
//!
//! What counts as a port reference: an integer under a `port` key, an `/imposters/<n>` path
//! segment, and a `localhost:<n>` / `127.0.0.1:<n>` authority or `/imposters/<n>` segment inside
//! any string (proxy `to`, `defaultForward`, a recorded `Host` header, a `Location` header). The engine's admin port is
//! reverse-mapped to `2525` so admin URLs compare equal too.

use regex::{Captures, Regex};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The admin port both engines are presented as once their output is reverse-mapped.
pub const LOGICAL_ADMIN_PORT: u16 = 2525;

static AUTHORITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(localhost|127\.0\.0\.1|\[::1\]):(\d{1,5})").expect("authority regex is valid")
});
static IMPOSTER_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/imposters/(\d{1,5})").expect("imposter path regex is valid"));
static IMPOSTER_SEGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/imposters/(\d{1,5})").expect("imposter segment regex is valid"));

/// One engine's view of the case's ports.
#[derive(Debug, Clone, Default)]
pub struct PortMap {
    forward: BTreeMap<u16, u16>,
    reverse: BTreeMap<u16, u16>,
}

impl PortMap {
    /// Builds a map from `(logical, actual)` pairs plus the engine's admin port.
    #[must_use]
    pub fn new(pairs: impl IntoIterator<Item = (u16, u16)>, admin_actual: u16) -> Self {
        let mut map = Self::default();
        for (logical, actual) in pairs {
            map.insert(logical, actual);
        }
        map.reverse.insert(admin_actual, LOGICAL_ADMIN_PORT);
        map
    }

    /// Registers a port the engine chose itself (an imposter created without `port`).
    pub fn insert(&mut self, logical: u16, actual: u16) {
        self.forward.insert(logical, actual);
        self.reverse.insert(actual, logical);
    }

    /// The actual port for `logical`, or `logical` itself when the case never declared it (a dead
    /// upstream such as `localhost:9999` must stay dead on both engines).
    #[must_use]
    pub fn actual(&self, logical: u16) -> u16 {
        self.forward.get(&logical).copied().unwrap_or(logical)
    }

    #[must_use]
    pub fn forward_value(&self, value: &Value) -> Value {
        map_value(value, &self.forward)
    }

    #[must_use]
    pub fn reverse_value(&self, value: &Value) -> Value {
        map_value(value, &self.reverse)
    }

    #[must_use]
    pub fn forward_str(&self, text: &str) -> String {
        map_authorities(text, &self.forward)
    }

    #[must_use]
    pub fn reverse_str(&self, text: &str) -> String {
        map_authorities(text, &self.reverse)
    }

    /// Maps the `/imposters/<n>` segment of an admin path; other paths pass through.
    #[must_use]
    pub fn forward_path(&self, path: &str) -> String {
        IMPOSTER_PATH
            .replace(path, |caps: &Captures<'_>| match caps[1].parse::<u16>() {
                Ok(logical) => format!("/imposters/{}", self.actual(logical)),
                Err(_) => caps[0].to_string(),
            })
            .into_owned()
    }
}

/// Every logical port a JSON document references: `port` integers and authorities in strings.
pub fn collect_ports(value: &Value, out: &mut std::collections::BTreeSet<u16>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "port"
                    && let Some(port) = child.as_u64().and_then(|p| u16::try_from(p).ok())
                {
                    out.insert(port);
                }
                collect_ports(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_ports(item, out)),
        Value::String(text) => {
            for caps in AUTHORITY.captures_iter(text) {
                if let Ok(port) = caps[2].parse::<u16>() {
                    out.insert(port);
                }
            }
        }
        _ => {}
    }
}

/// The logical port named by an `/imposters/<n>` admin path, if any.
#[must_use]
pub fn path_port(path: &str) -> Option<u16> {
    IMPOSTER_PATH
        .captures(path)
        .and_then(|caps| caps[1].parse::<u16>().ok())
}

fn map_value(value: &Value, table: &BTreeMap<u16, u16>) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| {
                    let mapped = match (key.as_str(), child.as_u64()) {
                        ("port", Some(port)) => u16::try_from(port)
                            .ok()
                            .and_then(|p| table.get(&p))
                            .map_or_else(|| child.clone(), |p| Value::from(*p)),
                        _ => map_value(child, table),
                    };
                    (key.clone(), mapped)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| map_value(v, table)).collect()),
        Value::String(text) => Value::String(map_authorities(text, table)),
        other => other.clone(),
    }
}

fn map_authorities(text: &str, table: &BTreeMap<u16, u16>) -> String {
    let authorities = AUTHORITY.replace_all(text, |caps: &Captures<'_>| {
        match caps[2].parse::<u16>().ok().and_then(|p| table.get(&p)) {
            Some(mapped) => format!("{}:{mapped}", &caps[1]),
            None => caps[0].to_string(),
        }
    });
    IMPOSTER_SEGMENT
        .replace_all(&authorities, |caps: &Captures<'_>| {
            match caps[1].parse::<u16>().ok().and_then(|p| table.get(&p)) {
                Some(mapped) => format!("/imposters/{mapped}"),
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map() -> PortMap {
        PortMap::new([(4545, 50001), (4546, 50002)], 50000)
    }

    #[test]
    fn forward_rewrites_port_fields_paths_and_authorities_only_for_declared_ports() {
        let imposter = json!({
            "port": 4545,
            "stubs": [{"responses": [{"proxy": {"to": "http://localhost:4546/x"}}]},
                      {"responses": [{"proxy": {"to": "http://localhost:9999"}}]}]
        });
        assert_eq!(
            map().forward_value(&imposter),
            json!({
                "port": 50001,
                "stubs": [{"responses": [{"proxy": {"to": "http://localhost:50002/x"}}]},
                          {"responses": [{"proxy": {"to": "http://localhost:9999"}}]}]
            })
        );
        assert_eq!(
            map().forward_path("/imposters/4545/stubs/0"),
            "/imposters/50001/stubs/0"
        );
        assert_eq!(map().forward_path("/imposters"), "/imposters");
    }

    #[test]
    fn reverse_maps_actual_ports_and_the_admin_port_back() {
        let recorded = json!({"port": 50001, "headers": {"Host": "127.0.0.1:50001"},
                              "location": "http://localhost:50000/imposters/50001"});
        assert_eq!(
            map().reverse_value(&recorded),
            json!({"port": 4545, "headers": {"Host": "127.0.0.1:4545"},
                   "location": "http://localhost:2525/imposters/4545"})
        );
        assert_eq!(
            map().reverse_str("Location: http://localhost:50000/"),
            "Location: http://localhost:2525/"
        );
    }

    #[test]
    fn collect_ports_finds_port_fields_and_authorities() {
        let mut found = std::collections::BTreeSet::new();
        collect_ports(
            &json!({"port": 4501, "stubs": [{"responses": [{"proxy": {"to": "http://localhost:4502"}}]}]}),
            &mut found,
        );
        assert_eq!(found.into_iter().collect::<Vec<_>>(), vec![4501, 4502]);
        assert_eq!(path_port("/imposters/4545/stubs"), Some(4545));
        assert_eq!(path_port("/imposters"), None);
    }
}
