//! Mountebank-oracle differential harness (issue #1341).
//!
//! Every case is replayed against a real Mountebank and against the Rift binary under test, and
//! the only assertion is that both engines answered the same: every response (status, headers,
//! body) and, after the sequence, every stored imposter in both its plain and replayable form. A
//! difference fails the case unless an entry in `allowlist.json` names that exact case and location
//! and either cites the documentation line that declares the deviation or names the Rift bug.
//!
//! `tests/differential.rs` is the entry point; this library holds the pieces it is built from so
//! the canonicalisation and diff rules are unit-tested on literal inputs.

pub mod allow;
pub mod canon;
pub mod case;
pub mod diff;
pub mod driver;
pub mod engine;
pub mod ports;
