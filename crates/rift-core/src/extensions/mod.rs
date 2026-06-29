//! Rift Extensions - Features beyond Mountebank compatibility.
//!
//! This module contains Rift's value-add features that go beyond standard
//! Mountebank functionality:
//!
//! - **Fault Injection** (`fault`): Probabilistic fault injection with latency,
//!   error responses, and TCP-level faults
//! - **Flow State** (`flow_state`): Stateful testing with in-memory or Redis backends
//! - **Rule Matching** (`matcher`): Enhanced request matching with compiled predicates
//! - **Metrics** (`metrics`): Prometheus metrics for observability
//! - **Rule Indexing** (`rule_index`): High-performance rule lookup using radix tries
//! - **Stub Analysis** (`stub_analysis`): Conflict detection and overlap warnings
//! - **Template** (`template`): Response body templating with request data
//! - **Routing** (`routing`): Multi-upstream routing for reverse proxy mode

pub mod fault;
pub mod flow_state;
pub mod matcher;
pub mod metrics;
pub mod routing;
pub mod rule_index;
pub mod stub_analysis;
pub mod template;

// Re-export commonly used types for library consumers
#[allow(unused_imports)]
pub use fault::{create_error_response, decide_fault, FaultDecision};
#[allow(unused_imports)]
pub use flow_state::{create_flow_store, FlowStore, NoOpFlowStore};
#[allow(unused_imports)]
pub use matcher::{CompiledMatch, CompiledRule};
#[allow(unused_imports)]
pub use metrics::{collect_metrics, record_request};
#[allow(unused_imports)]
pub use routing::Router;
#[allow(unused_imports)]
pub use rule_index::{IndexedRule, RuleIndex};
#[allow(unused_imports)]
pub use stub_analysis::{
    analyze_new_stub, analyze_stubs, StubAnalysisResult, StubWarning, WarningType,
};
#[allow(unused_imports)]
pub use template::{process_template, RequestData};
