//! Routing configuration for reverse proxy mode.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Route {
    pub name: String,
    #[serde(rename = "match")]
    pub match_config: RouteMatch,
    pub upstream: String, // upstream name
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RouteMatch {
    #[serde(default)]
    pub host: Option<HostMatch>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub path_exact: Option<String>,
    #[serde(default)]
    pub path_regex: Option<String>,
    #[serde(default)]
    pub headers: Vec<HeaderMatch>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum HostMatch {
    Exact(String),
    Wildcard { wildcard: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeaderMatch {
    pub name: String,
    pub value: String,
}
