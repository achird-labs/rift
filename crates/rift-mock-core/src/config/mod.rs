//! Configuration types shared by the imposter path.
//!
//! This module used to also define `Config` — the reverse-proxy / sidecar YAML mode with
//! `upstreams`, `routing`, `rules` and `recording`. That mode was unwired from the binary in
//! ada6f30 (2025-11-30), had no consumer in any repo, and was removed in #975. What is left is the
//! flow-state vocabulary the imposter path reads. The fault vocabulary that sat here alongside it
//! was retained as public API "pending its own decision"; #1000 is that decision — it had no
//! in-tree reader (the imposter path uses its own `_rift.fault` type) and went with
//! `extensions::fault`.

mod scripting;

pub use scripting::FlowStateConfig;

#[cfg(test)]
mod tests {
    use super::*;

    // NB: unlike `ImposterConfig`, none of these carry `rename_all = "camelCase"`, so their
    // wire names are snake_case. That asymmetry is easy to "fix" by accident.
    // Issue #975: the survivors of the reverse-proxy `Config` removal. `FlowStateConfig` is
    // read by the flow-state path; the fault types are retained public API with
    // no in-tree reader. Pinning the wire shapes with literal expectations guards the one plausible
    // way the deletion could go wrong — taking a live type with it, or silently changing what its
    // serde attributes accept.
    // A config-file `flowState` written before the Redis backend was removed still decodes: the
    // removal is an admission refusal (`create_flow_store`), never a decode failure (#1337).
    #[test]
    fn flow_state_config_still_decodes_a_leftover_redis_block() {
        let cfg: FlowStateConfig = serde_norway::from_str(
            "backend: redis\nttl_seconds: 42\nredis:\n  url: redis://127.0.0.1:6379\n  pool_size: 3\n",
        )
        .expect("FlowStateConfig parses");
        assert_eq!(cfg.backend, "redis");
        assert_eq!(cfg.ttl_seconds, 42);
    }

    #[test]
    fn flow_state_config_defaults_are_unchanged() {
        let cfg: FlowStateConfig = serde_norway::from_str("{}").expect("empty parses");
        assert_eq!(cfg.backend, "inmemory");
        assert_eq!(cfg.ttl_seconds, 300);
    }
}
