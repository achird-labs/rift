//! Built-in [`FlowStore`](crate::extensions::flow_state::FlowStore) backends.
//!
//! Only the in-memory store lives here. The Redis backend moved to the `rift-store-redis` crate
//! (issue #853) and attaches at runtime through
//! [`FlowStoreBackendFactory`](crate::extensions::flow_state::FlowStoreBackendFactory), which is
//! what keeps `redis`/`r2d2` out of this crate's dependency graph entirely.

pub mod inmemory;

pub use inmemory::InMemoryFlowStore;
