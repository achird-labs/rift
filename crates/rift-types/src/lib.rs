//! Shared core types for the Rift workspace.
//!
//! Pure, serde-friendly data types with no behaviour, so they can be depended on by
//! `rift-http-proxy`, `rift-lint`, and `rift-tui` without circular dependencies. The one
//! exception is [`wire`], which carries the shared serde rules for how those types are spelled on
//! the wire — it lives here for the same reason the types do: so no two crates can disagree.

pub mod predicate;
#[cfg(feature = "schema")]
pub mod schema;
pub mod wire;

pub use predicate::{
    PREDICATE_GENERATOR_KEYS, PREDICATE_OPERATORS, PREDICATE_PARAMETERS, Predicate,
    PredicateOperation, PredicateParameters, PredicateSelector,
};
