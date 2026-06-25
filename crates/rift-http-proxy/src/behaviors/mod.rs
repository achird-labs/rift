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
pub use copy::{apply_copy_behaviors, CopyBehavior, CopySource};
pub use cycler::{HasRepeatBehavior, ResponseCycler, RuleCycler};
#[allow(unused_imports)]
pub use extraction::{extract_jsonpath, extract_xpath, extract_xpath_with_ns, ExtractionMethod};
#[allow(unused_imports)]
pub use lookup::{
    apply_lookup_behaviors, CsvCache, CsvData, CsvDataSource, DataSource, LookupBehavior, LookupKey,
};
pub use request::{header_to_title_case, RequestContext};
pub use transform::{apply_decorate, apply_shell_transform};
pub use types::ResponseBehaviors;
#[allow(unused_imports)]
pub use wait::WaitBehavior;
