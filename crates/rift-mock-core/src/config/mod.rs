//! Configuration types shared by the imposter path.
//!
//! This module used to also define `Config` — the reverse-proxy / sidecar YAML mode with
//! `upstreams`, `routing`, `rules` and `recording`. That mode was unwired from the binary in
//! ada6f30 (2025-11-30), had no consumer in any repo, and was removed in #975. What is left is the
//! flow-state vocabulary the imposter path reads, plus the fault vocabulary — which has no in-tree
//! reader (the imposter path uses its own `_rift.fault` type) and is retained as public API pending
//! its own decision.

mod rules;
mod scripting;

pub use rules::{ErrorFault, FaultConfig, LatencyFault, TcpFault};
pub use scripting::{FlowStateConfig, RedisConfig};

#[cfg(test)]
mod tests {
    use super::*;

    // NB: unlike `ImposterConfig`, none of these carry `rename_all = "camelCase"`, so their
    // wire names are snake_case. That asymmetry is easy to "fix" by accident.
    // Issue #975: the survivors of the reverse-proxy `Config` removal. `FlowStateConfig` /
    // `RedisConfig` are read by the flow-state path; the fault types are retained public API with
    // no in-tree reader. Pinning the wire shapes with literal expectations guards the one plausible
    // way the deletion could go wrong — taking a live type with it, or silently changing what its
    // serde attributes accept.
    #[test]
    fn flow_state_config_still_deserializes_with_a_nested_redis_block() {
        let cfg: FlowStateConfig = serde_yaml::from_str(
            "backend: redis\nttl_seconds: 42\nredis:\n  url: redis://127.0.0.1:6379\n  pool_size: 3\n  key_prefix: 'rift:'\n",
        )
        .expect("FlowStateConfig parses");
        assert_eq!(cfg.backend, "redis");
        assert_eq!(cfg.ttl_seconds, 42);
        let redis = cfg.redis.expect("redis block present");
        assert_eq!(redis.url, "redis://127.0.0.1:6379");
        assert_eq!(redis.pool_size, 3);
        assert_eq!(redis.key_prefix, "rift:");
    }

    #[test]
    fn flow_state_config_defaults_are_unchanged() {
        let cfg: FlowStateConfig = serde_yaml::from_str("{}").expect("empty parses");
        assert_eq!(cfg.backend, "inmemory");
        assert_eq!(cfg.ttl_seconds, 300);
        assert!(cfg.redis.is_none());
    }

    #[test]
    fn tcp_fault_still_deserializes_its_screaming_snake_names() {
        let reset: TcpFault =
            serde_yaml::from_str("CONNECTION_RESET_BY_PEER").expect("reset parses");
        assert_eq!(reset, TcpFault::ConnectionResetByPeer);
        let garbage: TcpFault =
            serde_yaml::from_str("RANDOM_DATA_THEN_CLOSE").expect("garbage parses");
        assert_eq!(garbage, TcpFault::RandomDataThenClose);
    }

    #[test]
    fn fault_config_still_carries_latency_and_error() {
        let cfg: FaultConfig = serde_yaml::from_str(
            "latency:\n  probability: 0.5\n  min_ms: 10\n  max_ms: 20\nerror:\n  probability: 1.0\n  status: 503\n",
        )
        .expect("FaultConfig parses");
        let latency = cfg.latency.expect("latency present");
        assert_eq!(latency.probability, 0.5);
        assert_eq!(latency.min_ms, 10);
        assert_eq!(latency.max_ms, 20);
        assert_eq!(cfg.error.expect("error present").status, 503);
    }

    #[test]
    fn fault_config_reads_tcp_fault_under_its_snake_case_name() {
        // `tcp_fault`, not `tcpFault`: the field most likely to be "corrected" by someone applying
        // `ImposterConfig`'s camelCase convention to a struct that does not carry it.
        let cfg: FaultConfig =
            serde_yaml::from_str("tcp_fault: CONNECTION_RESET_BY_PEER\n").expect("parses");
        assert_eq!(cfg.tcp_fault, Some(TcpFault::ConnectionResetByPeer));
        let camel: FaultConfig = serde_yaml::from_str("tcpFault: CONNECTION_RESET_BY_PEER\n")
            .expect("unknown keys are ignored, so this parses");
        assert!(
            camel.tcp_fault.is_none(),
            "camelCase must NOT bind — proving the snake_case name above is load-bearing"
        );
    }
}
