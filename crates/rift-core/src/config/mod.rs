//! Configuration types for Rift proxy.

mod listen;
mod protocol;
mod recording;
mod routing;
mod rules;
mod scripting;
mod upstream;

use std::path::Path;

use serde::{Deserialize, Serialize};

// Re-export all types for library consumers
#[allow(unused_imports)]
pub use listen::{ListenConfig, MetricsConfig, TlsConfig};
pub use protocol::{DeploymentMode, Protocol};
#[allow(unused_imports)]
pub use recording::{
    PredicateGenerator, PredicateGeneratorMatches, RecordingConfig, RecordingPersistence,
};
#[allow(unused_imports)]
pub use routing::{HeaderMatch, HostMatch, Route, RouteMatch};
#[allow(unused_imports)]
pub use rules::{
    ErrorFault, FaultConfig, LatencyFault, MatchConfig, PathMatch, Rule, ScriptRule, TcpFault,
};
#[allow(unused_imports)]
pub use scripting::{
    DecisionCacheConfigFile, FlowStateConfig, RedisConfig, ScriptEngineConfig, ScriptPoolConfigFile,
};
#[allow(unused_imports)]
pub use upstream::{ConnectionPoolConfig, HealthCheckConfig, Upstream, UpstreamConfig};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Optional, informational only. The config is self-describing and supports
    /// combining features (probabilistic rules, script rules, multi-upstream).
    /// Deprecated: kept for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Deployment mode: "sidecar" or "reverse-proxy"
    /// Recommended: specify explicitly for clarity
    /// If omitted, inferred from upstream/upstreams presence (backward compatible)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<DeploymentMode>,

    pub listen: ListenConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,

    // ===== Deployment Mode Configuration =====
    // Choose exactly ONE deployment mode:
    //
    // SIDECAR MODE:
    //   - Define 'upstream' (single target)
    //   - Do NOT define 'upstreams' or 'routing'
    //   - Traffic: Client -> Rift -> Single Upstream
    //
    // REVERSE PROXY MODE:
    //   - Define 'upstreams' (list of named services)
    //   - Define 'routing' (map requests to upstream names)
    //   - Do NOT define 'upstream'
    //   - Traffic: Client -> Rift -> Multiple Upstreams (routed)
    /// Single upstream target for sidecar mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamConfig>,

    /// Multiple upstream targets for reverse proxy mode
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<Upstream>,

    /// Routing rules for reverse proxy mode (required when 'upstreams' is used)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routing: Vec<Route>,

    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub script_engine: Option<ScriptEngineConfig>,
    #[serde(default)]
    pub flow_state: Option<FlowStateConfig>,
    #[serde(default)]
    pub script_rules: Vec<ScriptRule>,
    #[serde(default)]
    pub connection_pool: ConnectionPoolConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_pool: Option<ScriptPoolConfigFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_cache: Option<DecisionCacheConfigFile>,
    /// Recording configuration for proxy record/replay (Mountebank-compatible)
    #[serde(default)]
    pub recording: RecordingConfig,
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, anyhow::Error> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        // Validate listener configuration
        if self.listen.protocol == Protocol::Https && self.listen.tls.is_none() {
            anyhow::bail!(
                "TLS configuration is required when listener protocol is 'https'. \
                 Please provide 'listen.tls.cert_path' and 'listen.tls.key_path'"
            );
        }

        // Validate listener protocol is supported
        if !self.listen.protocol.is_supported() {
            anyhow::bail!(
                "Unsupported listener protocol: '{}'. Currently supported: http, https",
                self.listen.protocol.as_str()
            );
        }

        // Validate upstream configuration (sidecar mode)
        if let Some(ref upstream) = self.upstream {
            let protocol = upstream.get_protocol();
            if !protocol.is_supported() {
                anyhow::bail!(
                    "Unsupported upstream protocol: '{}'. Currently supported: http, https",
                    protocol.as_str()
                );
            }
        }

        // Validate all upstreams (reverse proxy mode)
        for upstream in &self.upstreams {
            upstream.validate().map_err(|e| anyhow::anyhow!(e))?;
        }

        // Validate script rules if present
        self.validate_script_rules()?;

        Ok(())
    }

    /// Validate all script rules based on the configured script engine
    fn validate_script_rules(&self) -> Result<(), anyhow::Error> {
        if self.script_rules.is_empty() {
            return Ok(());
        }

        let engine_type = self
            .script_engine
            .as_ref()
            .map(|cfg| cfg.engine.as_str())
            .unwrap_or("rhai");

        for script_rule in &self.script_rules {
            match engine_type {
                "rhai" => {
                    use crate::scripting::{RhaiValidator, ScriptValidator};
                    RhaiValidator::new()
                        .validate(&script_rule.script)
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Invalid Rhai script in rule '{}': {}",
                                script_rule.id,
                                e
                            )
                        })?;
                }
                #[cfg(feature = "lua")]
                "lua" => {
                    use crate::scripting::{LuaValidator, ScriptValidator};
                    LuaValidator::new()
                        .validate(&script_rule.script)
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Invalid Lua script in rule '{}': {}",
                                script_rule.id,
                                e
                            )
                        })?;
                }
                #[cfg(not(feature = "lua"))]
                "lua" => {
                    anyhow::bail!("Lua engine specified but 'lua' feature is not enabled");
                }
                #[cfg(feature = "javascript")]
                "javascript" | "js" => {
                    use crate::scripting::{JsValidator, ScriptValidator};
                    JsValidator::new()
                        .validate(&script_rule.script)
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Invalid JavaScript script in rule '{}': {}",
                                script_rule.id,
                                e
                            )
                        })?;
                }
                #[cfg(not(feature = "javascript"))]
                "javascript" | "js" => {
                    anyhow::bail!(
                        "JavaScript engine specified but 'javascript' feature is not enabled"
                    );
                }
                other => {
                    anyhow::bail!("Unknown script engine type: '{other}'");
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::ProxyMode;

    #[test]
    fn test_parse_config() {
        let yaml = r#"
version: v1
listen:
  port: 8080
metrics:
  port: 9090
upstream:
  host: 127.0.0.1
  port: 8000
rules:
  - id: "test-latency"
    match:
      methods: ["POST"]
      path:
        prefix: "/api"
    fault:
      latency:
        probability: 0.1
        min_ms: 100
        max_ms: 500
  - id: "test-error"
    match:
      methods: ["GET"]
      path:
        exact: "/fail"
    fault:
      error:
        probability: 0.5
        status: 502
        body: '{"error": "injected"}'
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.version, Some("v1".to_string()));
        assert_eq!(config.listen.port, 8080);
        assert_eq!(config.upstream.as_ref().unwrap().port, 8000);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].id, "test-latency");
        assert!(config.rules[0].fault.latency.is_some());
        assert_eq!(config.rules[1].id, "test-error");
        assert!(config.rules[1].fault.error.is_some());
    }

    #[test]
    fn test_parse_v2_config() {
        let yaml = r#"
version: v2
listen:
  port: 8080
upstream:
  host: 127.0.0.1
  port: 8000
script_engine:
  engine: rhai
flow_state:
  backend: inmemory
  ttl_seconds: 300
script_rules:
  - id: "progressive-failure"
    script: |
      fn should_inject(request, flow_store) {
        let flow_id = request.headers["x-flow-id"];
        let attempts = flow_store.increment(flow_id, "attempts");
        if attempts <= 2 {
          return #{ inject: true, fault: "error", status: 503, body: "Retry" };
        }
        #{ inject: false }
      }
    match:
      methods: ["POST"]
      path:
        prefix: "/api"
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.version, Some("v2".to_string()));
        assert!(config.script_engine.is_some());
        assert_eq!(config.script_engine.unwrap().engine, "rhai");
        assert!(config.flow_state.is_some());
        assert_eq!(config.flow_state.as_ref().unwrap().backend, "inmemory");
        assert_eq!(config.flow_state.as_ref().unwrap().ttl_seconds, 300);
        assert_eq!(config.script_rules.len(), 1);
        assert_eq!(config.script_rules[0].id, "progressive-failure");
        assert!(config.script_rules[0].script.contains("should_inject"));
    }

    #[test]
    fn test_parse_v3_multi_upstream_config() {
        let yaml = r#"
version: v3
listen:
  port: 8080
upstreams:
  - name: service-a
    url: "http://service-a:8000"
    health_check:
      path: "/health"
      interval_seconds: 30
  - name: service-b
    url: "http://service-b:8001"
routing:
  - name: "route-to-a"
    match:
      path_prefix: "/api/users"
    upstream: service-a
  - name: "route-to-b"
    match:
      path_prefix: "/api/orders"
      headers:
        - name: "x-version"
          value: "v2"
    upstream: service-b
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.version, Some("v3".to_string()));

        // Verify multi-upstream mode
        assert!(config.upstream.is_none());
        assert_eq!(config.upstreams.len(), 2);
        assert_eq!(config.upstreams[0].name, "service-a");
        assert_eq!(config.upstreams[0].url, "http://service-a:8000");
        assert!(config.upstreams[0].health_check.is_some());
        assert_eq!(config.upstreams[1].name, "service-b");

        // Verify routing
        assert_eq!(config.routing.len(), 2);
        assert_eq!(config.routing[0].name, "route-to-a");
        assert_eq!(config.routing[0].upstream, "service-a");
        assert_eq!(
            config.routing[0].match_config.path_prefix,
            Some("/api/users".to_string())
        );

        assert_eq!(config.routing[1].name, "route-to-b");
        assert_eq!(config.routing[1].upstream, "service-b");
        assert_eq!(config.routing[1].match_config.headers.len(), 1);
        assert_eq!(config.routing[1].match_config.headers[0].name, "x-version");
    }

    #[test]
    fn test_parse_error_fault_with_headers() {
        let yaml = r#"
version: v1
listen:
  port: 8080
upstream:
  host: 127.0.0.1
  port: 8000
rules:
  - id: "error-with-headers"
    match:
      methods: ["GET"]
      path:
        prefix: "/api"
    fault:
      error:
        probability: 1.0
        status: 502
        body: '{"error":"Service unavailable"}'
        headers:
          Server: "openresty"
          X-Content-Type-Options: "nosniff"
          Cache-Control: "no-cache, no-store, max-age=0, must-revalidate"
          x-apigw-key: "CapiOne-IT-INT"
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].id, "error-with-headers");

        let error_fault = config.rules[0].fault.error.as_ref().unwrap();
        assert_eq!(error_fault.status, 502);
        assert_eq!(error_fault.headers.len(), 4);
        assert_eq!(
            error_fault.headers.get("Server"),
            Some(&"openresty".to_string())
        );
        assert_eq!(
            error_fault.headers.get("X-Content-Type-Options"),
            Some(&"nosniff".to_string())
        );
        assert_eq!(
            error_fault.headers.get("x-apigw-key"),
            Some(&"CapiOne-IT-INT".to_string())
        );
    }

    #[test]
    fn test_parse_per_upstream_fault_rules() {
        let yaml = r#"
version: v3
listen:
  port: 8080
upstreams:
  - name: service-a
    url: "http://service-a:8000"
  - name: service-b
    url: "http://service-b:8001"
routing:
  - name: "route-a"
    match:
      path_prefix: "/api/a"
    upstream: service-a
  - name: "route-b"
    match:
      path_prefix: "/api/b"
    upstream: service-b
rules:
  # Global rule (applies to all upstreams)
  - id: "global-latency"
    match:
      methods: ["GET"]
    fault:
      latency:
        probability: 0.1
        min_ms: 100
        max_ms: 200
  # Service-specific rule (only applies to service-a)
  - id: "service-a-error"
    upstream: service-a
    match:
      methods: ["POST"]
    fault:
      error:
        probability: 0.5
        status: 503
        body: "Service A unavailable"
  # Another service-specific rule (only applies to service-b)
  - id: "service-b-latency"
    upstream: service-b
    match:
      path:
        prefix: "/api/b"
    fault:
      latency:
        probability: 0.8
        min_ms: 500
        max_ms: 1000
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.version, Some("v3".to_string()));

        // Verify rules with upstream filters
        assert_eq!(config.rules.len(), 3);

        // Global rule - no upstream filter
        assert_eq!(config.rules[0].id, "global-latency");
        assert!(config.rules[0].upstream.is_none());

        // Service-specific rules
        assert_eq!(config.rules[1].id, "service-a-error");
        assert_eq!(config.rules[1].upstream.as_ref().unwrap(), "service-a");
        assert!(config.rules[1].fault.error.is_some());

        assert_eq!(config.rules[2].id, "service-b-latency");
        assert_eq!(config.rules[2].upstream.as_ref().unwrap(), "service-b");
        assert!(config.rules[2].fault.latency.is_some());
    }

    #[test]
    fn test_parse_mountebank_behaviors() {
        let yaml = r#"
listen:
  port: 8080
upstream:
  host: localhost
  port: 9000
rules:
  - id: "behavior-wait-fixed"
    match:
      path:
        prefix: "/wait-fixed"
    fault:
      error:
        probability: 1.0
        status: 200
        body: '{"result": "delayed"}'
        behaviors:
          wait: 100
  - id: "behavior-wait-range"
    match:
      path:
        prefix: "/wait-range"
    fault:
      error:
        probability: 1.0
        status: 200
        body: '{"result": "delayed-range"}'
        behaviors:
          wait:
            min: 50
            max: 150
  - id: "tcp-reset"
    match:
      path:
        prefix: "/tcp-reset"
    fault:
      tcp_fault: CONNECTION_RESET_BY_PEER
  - id: "tcp-random"
    match:
      path:
        prefix: "/tcp-random"
    fault:
      tcp_fault: RANDOM_DATA_THEN_CLOSE
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.rules.len(), 4);

        // Test wait behavior - fixed
        let rule1 = &config.rules[0];
        assert_eq!(rule1.id, "behavior-wait-fixed");
        let error1 = rule1.fault.error.as_ref().unwrap();
        assert!(error1.behaviors.is_some());
        let behaviors1 = error1.behaviors.as_ref().unwrap();
        assert!(behaviors1.wait.is_some());

        // Test wait behavior - range
        let rule2 = &config.rules[1];
        assert_eq!(rule2.id, "behavior-wait-range");
        let error2 = rule2.fault.error.as_ref().unwrap();
        assert!(error2.behaviors.is_some());
        let behaviors2 = error2.behaviors.as_ref().unwrap();
        assert!(behaviors2.wait.is_some());

        // Test TCP fault - connection reset
        let rule3 = &config.rules[2];
        assert_eq!(rule3.id, "tcp-reset");
        assert!(rule3.fault.tcp_fault.is_some());
        assert_eq!(
            rule3.fault.tcp_fault.unwrap(),
            TcpFault::ConnectionResetByPeer
        );

        // Test TCP fault - random data
        let rule4 = &config.rules[3];
        assert_eq!(rule4.id, "tcp-random");
        assert!(rule4.fault.tcp_fault.is_some());
        assert_eq!(
            rule4.fault.tcp_fault.unwrap(),
            TcpFault::RandomDataThenClose
        );
    }

    #[test]
    fn test_parse_recording_config_proxy_once() {
        let yaml = r#"
listen:
  port: 8080
upstream:
  host: 127.0.0.1
  port: 8000
recording:
  mode: proxyOnce
rules: []
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.recording.mode, ProxyMode::ProxyOnce);
    }

    #[test]
    fn test_parse_recording_config_proxy_always() {
        let yaml = r#"
listen:
  port: 8080
upstream:
  host: 127.0.0.1
  port: 8000
recording:
  mode: proxyAlways
rules: []
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.recording.mode, ProxyMode::ProxyAlways);
    }

    #[test]
    fn test_parse_recording_config_default_transparent() {
        let yaml = r#"
listen:
  port: 8080
upstream:
  host: 127.0.0.1
  port: 8000
rules: []
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        // Default should be proxyTransparent
        assert_eq!(config.recording.mode, ProxyMode::ProxyTransparent);
    }

    #[test]
    fn test_parse_recording_config_explicit_transparent() {
        let yaml = r#"
listen:
  port: 8080
upstream:
  host: 127.0.0.1
  port: 8000
recording:
  mode: proxyTransparent
rules: []
"#;

        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.recording.mode, ProxyMode::ProxyTransparent);
    }
}
