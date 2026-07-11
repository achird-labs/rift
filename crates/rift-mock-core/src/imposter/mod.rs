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
mod fault_io;
mod handler;
mod manager;
pub(crate) mod predicates;
mod reconcile;
mod response;
mod script_resolve;
mod types;

#[cfg(test)]
mod tests;

// Re-export public types (used by external consumers like admin_api)
#[allow(unused_imports)]
pub use types::{
    DebugImposter, DebugMatchResult, DebugRequest, DebugResponse, DebugResponsePreview,
    DebugStubInfo, ImposterConfig, ImposterError, IsResponse, PathRewrite, Predicate,
    PredicateOperation, PredicateParameters, PredicateSelector, ProxyResponse, RecordedRequest,
    ResponseMode, RiftConfig, RiftConnectionPoolConfig, RiftErrorFault, RiftFaultConfig,
    RiftFlowStateConfig, RiftLatencyFault, RiftMetricsConfig, RiftProxyConfig, RiftRedisConfig,
    RiftResponseExtension, RiftScriptConfig, RiftScriptEngineConfig, RiftTcpFault,
    RiftUpstreamConfig, Stub, StubResponse,
};

// Re-export script `file:`/`ref:` resolution (issue #356)
pub use script_resolve::{
    ScriptBaseDir, ScriptResolveError, resolve_scripts, resolve_stub_scripts,
};

// Re-export core imposter
#[allow(unused_imports)]
pub mod journal;
pub use journal::{JournalRead, LocalJournal, RequestJournal};

pub use core::Imposter;
pub use core::{ClosestMatch, FailedPredicate, VerifyOptions, VerifyOutcome};

// Re-export the imposter request handler (single-port gateway dispatch, issue #212)
pub use handler::{handle_imposter_request, handle_imposter_request_decorated};

// Re-export the embedder flow-store provider hook (issue #312)
pub use crate::extensions::flow_state::FlowStoreProvider;

// Re-export manager
pub use manager::{ImposterManager, TlsDefaults};

// Re-export incremental reconciliation types (issue #316)
pub use reconcile::{ApplyReport, ImposterEvent, ImposterEventListener, stub_key};

// Re-export predicate utilities (used in tests and for external consumers)
#[allow(unused_imports)]
pub use predicates::{parse_query_string, predicate_matches, stub_matches};

// Re-export response utilities
#[allow(unused_imports)]
pub use response::create_response_preview;
