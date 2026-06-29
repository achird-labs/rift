//! Mountebank-compatible imposter management for Rift.
//!
//! This module provides:
//! - `ImposterManager`: Lifecycle management for imposters
//! - `Imposter`: Individual imposter with its own port, rules, and state
//! - `ImposterConfig`: Configuration for creating imposters
//!
//! Each imposter binds to its own TCP port and maintains isolated state.
//!
//! ## Module Structure
//!
//! - `types`: All type definitions (structs, enums, errors)
//! - `predicates`: Predicate matching logic for stub matching
//! - `response`: Response building and execution
//! - `handler`: HTTP request handling for imposters
//! - `manager`: ImposterManager for lifecycle management
//! - `core`: Core Imposter struct and implementation

mod core;
mod handler;
mod manager;
mod predicates;
mod response;
mod types;

#[cfg(test)]
mod tests;

// Re-export public types (used by external consumers like admin_api)
#[allow(unused_imports)]
pub use types::{
    DebugImposter, DebugMatchResult, DebugRequest, DebugResponse, DebugResponsePreview,
    DebugStubInfo, ImposterConfig, ImposterError, IsResponse, MountebankStateMapping, PathRewrite,
    Predicate, PredicateOperation, PredicateParameters, PredicateSelector, ProxyResponse,
    RecordedRequest, ResponseMode, RiftConfig, RiftConnectionPoolConfig, RiftErrorFault,
    RiftFaultConfig, RiftFlowStateConfig, RiftLatencyFault, RiftMetricsConfig, RiftProxyConfig,
    RiftRedisConfig, RiftResponseExtension, RiftScriptConfig, RiftScriptEngineConfig,
    RiftUpstreamConfig, Stub, StubResponse,
};

// Re-export core imposter
#[allow(unused_imports)]
pub use core::Imposter;

// Re-export manager
pub use manager::ImposterManager;

// Re-export predicate utilities (used in tests and for external consumers)
#[allow(unused_imports)]
pub use predicates::{parse_query_string, predicate_matches, stub_matches};

// Re-export response utilities
#[allow(unused_imports)]
pub use response::create_response_preview;
