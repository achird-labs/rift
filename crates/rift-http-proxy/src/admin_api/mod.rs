//! Admin REST API for Rift proxy and imposter management.
//!
//! This module provides a Mountebank-compatible REST API for:
//! - Creating, deleting, and listing imposters
//! - Managing stubs within imposters
//! - Clearing recorded requests and proxy responses
//! - Health and metrics endpoints
//!
//! The API listens on a configurable port (default: 2525).

mod handlers;
mod request_filter;
mod router;
mod server;
pub mod types;

pub use handlers::imposters::{filter_proxy_responses, filter_proxy_stubs};
pub use server::{AdminApiServer, RunningAdminApi};

/// Default port for the Mountebank-compatible admin API.
pub const DEFAULT_ADMIN_PORT: u16 = 2525;
