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
    /// A proxy's `mode` string as the engine reads it (issue #1314): surrounding whitespace and
    /// case are ignored, an empty string is [`ProxyMode::ProxyOnce`] (Mountebank's default), and
    /// anything else is `None`. Every reader of the string goes through here, so the replay store,
    /// the recording gate and the stub placement can never disagree about one value.
    #[must_use]
    pub fn parse(mode: &str) -> Option<Self> {
        let mode = mode.trim();
        if mode.is_empty() || mode.eq_ignore_ascii_case("proxyOnce") {
            Some(Self::ProxyOnce)
        } else if mode.eq_ignore_ascii_case("proxyAlways") {
            Some(Self::ProxyAlways)
        } else if mode.eq_ignore_ascii_case("proxyTransparent") {
            Some(Self::ProxyTransparent)
        } else {
            None
        }
    }
}
