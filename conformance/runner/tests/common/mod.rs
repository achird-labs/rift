//! What the test binaries share: the one way a feature file is parsed, so the parse gate
//! (`features_parse.rs`) and the runner (`plane_a.rs`) can never disagree about a file.

use std::path::Path;

use cucumber::feature::Ext as _;
use cucumber::{gherkin, parser};

/// Parse a feature file exactly as the runner does: gherkin-rs with the default (English)
/// environment, then `Scenario Outline` examples expanded.
pub fn parse_feature(path: &Path) -> Result<gherkin::Feature, parser::Error> {
    gherkin::Feature::parse_path(path, gherkin::GherkinEnv::default())
        .map_err(parser::Error::from)
        .and_then(|f| f.expand_examples().map_err(parser::Error::from))
}
