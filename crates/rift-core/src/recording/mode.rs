//! Proxy recording mode definitions.

use serde::{Deserialize, Serialize};

/// Proxy recording mode (Mountebank-compatible)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::enum_variant_names)] // Keep Mountebank-compatible names
pub enum ProxyMode {
    /// Record first response, replay on subsequent matches
    ProxyOnce,
    /// Always proxy, record all responses (for later replay via `mb replay`)
    ProxyAlways,
    /// Always proxy, never record (default Rift behavior)
    #[default]
    ProxyTransparent,
}
