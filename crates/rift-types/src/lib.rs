//! Shared core types for the Rift workspace.
//!
//! Pure, serde-friendly data types with no behaviour, so they can be depended on by
//! `rift-http-proxy`, `rift-lint`, and `rift-tui` without circular dependencies. The one
//! exception is [`wire`], which carries the shared serde rules for how those types are spelled on
//! the wire — it lives here for the same reason the types do: so no two crates can disagree.

pub mod predicate;
pub mod wire;

pub use predicate::{Predicate, PredicateOperation, PredicateParameters, PredicateSelector};
