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
//! - **Stub Analysis** (`stub_analysis`): Conflict detection and overlap warnings
//! - **Template** (`template`): Response body templating with request data
//! - **Template Functions** (`template_fn`): Declarative `{{ function args | filter }}`
//!   response templating (issue #359)
//! - **Routing** (`routing`): Multi-upstream routing for reverse proxy mode
//! - **No-Match Interceptor** (`no_match`): Last-chance hook for a genuine no-match, before the
//!   defaultForward/defaultResponse/empty-200 fallthrough (issue #819)
//! - **Admin Authorization** (`authz`): Pluggable per-request admin-API authorization with parsed
//!   route context, consulted after authentication (issue #854)
//! - **Exchange Inspector** (`exchange_inspector`): Synchronous per-imposter hooks that see a
//!   request before matching and the response before it is written, and may replace either
//!   (issue #966)

pub mod authz;
pub mod decorate;
pub mod exchange_inspector;
pub mod fault;
pub mod flow_state;
pub mod matcher;
pub mod metrics;
pub mod no_match;
pub mod routing;
pub mod stub_analysis;
pub mod template;
pub mod template_fn;

// Re-export commonly used types for library consumers
#[allow(unused_imports)]
pub use exchange_inspector::{
    ExchangeInspector, ExchangeInspectorProvider, InspectRequest, InspectResponse, InspectVerdict,
};
#[allow(unused_imports)]
pub use fault::{FaultDecision, create_error_response, decide_fault};
#[allow(unused_imports)]
pub use flow_state::{
    CasOutcome, FlowStore, FlowStoreBackendFactory, FlowStoreBackends, FlowStoreProvider,
    NoOpFlowStore, create_flow_store,
};
#[allow(unused_imports)]
pub use matcher::{CompiledMatch, CompiledRule};
#[allow(unused_imports)]
pub use metrics::{AcceptErrorCounters, AcceptOutageGuard, collect_metrics, record_request};
#[allow(unused_imports)]
pub use no_match::{NoMatchContext, NoMatchDirective, NoMatchInterceptor};
#[allow(unused_imports)]
pub use routing::Router;
#[allow(unused_imports)]
pub use stub_analysis::{
    StubAnalysisResult, StubWarning, WarningType, analyze_new_stub, analyze_stubs,
};
#[allow(unused_imports)]
pub use template::{RequestData, apply_date_templates, contains_date_templates, process_template};
#[allow(unused_imports)]
pub use template_fn::{TemplateContext, render_templated};
