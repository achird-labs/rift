//! Proxy recording mode definitions.

use serde::{Deserialize, Serialize};

/// Proxy recording mode (Mountebank-compatible)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::enum_variant_names)] // Keep Mountebank-compatible names
pub enum ProxyMode {
    /// Record first response, replay on subsequent matches. The default when a proxy omits
    /// `mode`, as in Mountebank (issue #1314).
    #[default]
    ProxyOnce,
    /// Always proxy, record all responses (for later replay via `mb replay`)
    ProxyAlways,
    /// Always proxy, never record
    ProxyTransparent,
}

impl ProxyMode {
    /// Every mode and its canonical spelling — the form the docs, `rift-lint` and the JSON Schema
    /// name (issue #1342). [`Self::parse`] reads this table and [`Self::SPELLINGS`] is its first
    /// column, so there is one list.
    const TABLE: [(&'static str, Self); 3] = [
        ("proxyOnce", Self::ProxyOnce),
        ("proxyAlways", Self::ProxyAlways),
        ("proxyTransparent", Self::ProxyTransparent),
    ];

    /// The canonical spelling of every mode. [`Self::parse`] also takes each in any case and with
    /// surrounding whitespace, and `""` as [`ProxyMode::ProxyOnce`].
    pub const SPELLINGS: [&'static str; 3] = {
        let mut spellings = [""; 3];
        let mut i = 0;
        while i < spellings.len() {
            spellings[i] = Self::TABLE[i].0;
            i += 1;
        }
        spellings
    };

    /// A proxy's `mode` string as the engine reads it (issue #1314): surrounding whitespace and
    /// case are ignored, an empty string is [`ProxyMode::ProxyOnce`] (Mountebank's default), and
    /// anything else is `None`. Every reader of the string goes through here, so the replay store,
    /// the recording gate and the stub placement can never disagree about one value.
    #[must_use]
    pub fn parse(mode: &str) -> Option<Self> {
        let mode = mode.trim();
        if mode.is_empty() {
            return Some(Self::ProxyOnce);
        }
        Self::TABLE
            .iter()
            .find(|(spelling, _)| spelling.eq_ignore_ascii_case(mode))
            .map(|(_, parsed)| *parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::ProxyMode;

    /// `SPELLINGS` names every mode once, in its canonical spelling: each parses to a distinct
    /// mode, and each mode's spelling round-trips through serde to the same string.
    #[test]
    fn spellings_parse_to_their_mode() {
        let modes: Vec<ProxyMode> = ProxyMode::SPELLINGS
            .iter()
            .map(|s| ProxyMode::parse(s).unwrap_or_else(|| panic!("{s} does not parse")))
            .collect();
        assert_eq!(
            modes,
            [
                ProxyMode::ProxyOnce,
                ProxyMode::ProxyAlways,
                ProxyMode::ProxyTransparent
            ]
        );
        for (spelling, mode) in ProxyMode::SPELLINGS.iter().zip(modes) {
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::json!(spelling)
            );
        }
        assert_eq!(ProxyMode::parse(""), Some(ProxyMode::ProxyOnce));
        assert_eq!(
            ProxyMode::parse(" PROXYALWAYS "),
            Some(ProxyMode::ProxyAlways)
        );
    }
}
