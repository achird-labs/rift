//! Built-in [`FlowStore`](crate::extensions::flow_state::FlowStore) backends.
//!
//! Only the in-memory store lives here. Any other store attaches at runtime through
//! [`FlowStoreBackendFactory`](crate::extensions::flow_state::FlowStoreBackendFactory) or
//! [`FlowStoreProvider`](crate::extensions::flow_state::FlowStoreProvider).

pub mod inmemory;

pub use inmemory::InMemoryFlowStore;
