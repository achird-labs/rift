//! Mountebank-compatible response behaviors.
//!
//! This module implements the `_behaviors` functionality from Mountebank,
//! allowing dynamic response modification based on request data.
//!
//! # Supported Behaviors
//!
//! - `wait` - Add latency before response (fixed ms or {min, max} range)
//! - `repeat` - Repeat response N times before cycling to next
//! - `copy` - Copy request fields into response using regex/jsonpath/xpath
//! - `lookup` - Query external CSV data source
//! - `shellTransform` - External program transforms response
//! - `decorate` - Rhai script to post-process response

// Allow dead code for now as behaviors are designed for future integration
#![allow(dead_code)]

mod copy;
mod cycler;
mod extraction;
mod lookup;
mod request;
mod transform;
mod types;
mod wait;

// Re-export main types for library consumers
#[allow(unused_imports)]
pub use copy::{CopyBehavior, CopySource, apply_copy_behaviors};
pub use cycler::{HasRepeatBehavior, ResponseCycler, RuleCycler};

pub mod sequencer;
#[allow(unused_imports)]
pub use extraction::{ExtractionMethod, extract_jsonpath, extract_xpath, extract_xpath_with_ns};
#[allow(unused_imports)]
pub use lookup::{
    CsvCache, CsvData, CsvDataSource, DataSource, LookupBehavior, LookupKey, apply_lookup_behaviors,
};
pub use request::{RequestContext, header_to_title_case};
pub use sequencer::{LocalSequencer, ResponseSequencer, SequenceKey};
pub use transform::{
    DecorateError, apply_decorate, apply_shell_transform, is_js_config_decorate,
    rewrite_js_config_to_rhai,
};
pub use types::ResponseBehaviors;
#[allow(unused_imports)]
pub use wait::WaitBehavior;
