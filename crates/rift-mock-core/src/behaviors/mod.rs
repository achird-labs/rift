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

mod copy;
mod cycler;
mod extraction;
mod lookup;
mod request;
mod spliced;
mod transform;
mod types;
mod wait;
#[cfg(feature = "javascript")]
pub(crate) use wait::thrown_message;

// Re-export main types for library consumers
#[allow(unused_imports)]
pub use copy::{CopyBehavior, CopySource, apply_copy_behaviors};
pub use cycler::{HasRepeatBehavior, RuleCycler};

pub mod sequencer;
pub(crate) use copy::apply_copy_spliced;
#[cfg(test)]
pub(crate) use extraction::counters;
#[allow(unused_imports)]
pub use extraction::{
    ExtractionMethod, XPathSelectorError, extract_jsonpath, extract_jsonpath_value, extract_xpath,
    extract_xpath_with_ns,
};
pub(crate) use extraction::{
    LazyJson, LazyXmlDom, Selection, fold_xpath_selector, jsonpath_selection,
    validate_jsonpath_selector, validate_xpath_selector, xpath_selection, xpath_selection_in,
};
pub(crate) use lookup::apply_lookup_spliced;
#[allow(unused_imports)]
pub use lookup::{
    CsvCache, CsvData, CsvDataSource, DataSource, DatasetBinding, LookupBehavior, LookupKey,
    apply_lookup_behaviors,
};
pub use request::{RequestContext, header_to_title_case};
pub use sequencer::{LocalSequencer, ResponseSequencer, SequenceKey};
pub(crate) use spliced::{Spliced, SplicedHeaders, SplicedName, authored_headers, plain_headers};
pub use transform::{
    DecorateError, Decorated, MAX_SHELL_COMMAND_LENGTH, ResponseStatus, ReturnedOmits,
    ShellTransformError, apply_decorate, apply_shell_transform, is_javascript_decorate,
    is_js_config_decorate, is_js_function_decorate, rewrite_js_config_to_rhai, shell_command_line,
    shell_request_json,
};
pub use types::{BehaviorProgram, BehaviorStep, CANONICAL_ORDER, ResponseBehaviors};
#[allow(unused_imports)]
pub use wait::{WaitBehavior, WaitScriptError};
