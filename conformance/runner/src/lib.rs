//! Plane A conformance runner (issue #1346): the harness behind `tests/plane_a.rs`, which drives
//! the Gherkin specification in `conformance/features/rift/` against real `rift`, `rift-lint` and
//! `rift-verify` binaries.
//!
//! Ported from the Scala runner in rift-conformance (`RiftHarness.scala`, `RiftTools.scala`,
//! `RiftBuildInfo.scala`); the step definitions live in the test, this crate holds what they stand
//! on:
//!
//! - [`harness`]: the engine under test in one of three [`harness::Mode`]s, and the scenario-local
//!   engines some scenarios start beside it.
//! - [`tools`]: `rift-lint` / `rift-verify` run as subprocesses, their JSON parsed.
//! - [`build_info`]: the `BUILD_INFO` a staged build carries (feature 01's coherence gate).
//! - [`http`]: the HTTP clients the steps send with (plain, trust-all TLS, verifying TLS, mTLS).
//! - [`prom`]: Prometheus text exposition, parsed per series (feature 17's metric deltas).
//! - [`raw`]: what the HTTP client cannot show: bytes read off a bare socket, a two-stream h2c
//!   exchange, an over-cap body sent while the answer is read, a bare TCP connect.
//! - [`spec`]: where the specification lives and which features this runner has ported.
//! - [`tls`]: TLS clients trusting one test CA, and the chain and ALPN a handshake settled.
//! - [`wire`]: HTTP/1.1 and HTTP/2 framed by hand over one connection, for what a feature asserts
//!   about the wire itself.

pub mod build_info;
pub mod error;
pub mod harness;
pub mod http;
pub mod prom;
pub mod raw;
pub mod spec;
pub mod tls;
pub mod tools;
pub mod wire;

pub use error::HarnessError;
