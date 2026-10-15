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
//! - [`spec`]: where the specification lives and which features this runner has ported.

pub mod build_info;
pub mod error;
pub mod harness;
pub mod http;
pub mod spec;
pub mod tools;

pub use error::HarnessError;
