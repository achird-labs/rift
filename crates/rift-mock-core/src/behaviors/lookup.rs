//! Lookup behavior - query external data sources.

use super::copy::CopySource;
use super::extraction::ExtractionMethod;
use super::request::RequestContext;
use super::spliced::{Spliced, SplicedHeaders, authored_headers, plain_headers};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

/// Lookup behavior - query external data source
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LookupBehavior {
    /// Key extraction from request
    pub key: LookupKey,
    /// Data source configuration
    #[serde(rename = "fromDataSource")]
    pub from_data_source: DataSource,
    /// Token to replace in response (e.g., "${RESULT}")
    pub into: String,
}

/// Key extraction configuration for lookup
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LookupKey {
    /// Request field to extract key from
    pub from: CopySource,
    /// Extraction method
    #[serde(rename = "using")]
    pub extraction: ExtractionMethod,
    /// Which of the selector's matches keys the row, as in Mountebank (issue #1240): for a regex,
    /// `0` is the whole match and `n` the `n`th capture group; for JSONPath/XPath, the `n`th
    /// selected value. Absent keeps Rift's default, the first capture group (or the whole match
    /// when the pattern has none) and the first selected value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

impl LookupKey {
    /// The key this lookup reads the row by, or `None` when the request yields none.
    pub fn extract(&self, request: &RequestContext) -> Option<String> {
        let source = self.from.extract(request)?;
        match self.index {
            None => self.extraction.extract(&source),
            Some(index) => self
                .extraction
                .matches(&source)
                .into_iter()
                .nth(index)
                .flatten(),
        }
    }
}

/// External data source configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DataSource {
    /// CSV data source
    pub csv: CsvDataSource,
}

/// CSV data source configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CsvDataSource {
    /// Path to CSV file
    pub path: String,
    /// Column to use as lookup key
    #[serde(rename = "keyColumn")]
    pub key_column: String,
    /// Delimiter character (default: ',')
    #[serde(default = "default_delimiter")]
    pub delimiter: char,
}

fn default_delimiter() -> char {
    ','
}

/// A `_rift.dataset` block: a [`LookupBehavior`] named by *dataset* instead of by file path.
///
/// **Carried, never executed.** Nothing in this crate reads this type beyond serializing it back
/// out, and a standalone Rift serves a response bearing one exactly as if it were absent. It
/// exists because the path a `lookup` needs is node-local, so a clustered deployment cannot put
/// one in a config that replicates: `rift-cluster` resolves the name to a content-addressed file
/// on each node and rewrites this block into a real [`LookupBehavior`] at apply time. The engine
/// then runs the same unmodified lookup code it always has.
///
/// Keeping the declarative form in the config — rather than only the compiled result — is what
/// lets the cluster hand back the binding the operator actually wrote, and is why this type is
/// part of the config schema rather than an internal detail of the cluster.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DatasetBinding {
    /// The dataset's name, unique per tenant.
    pub name: String,
    /// Which version to bind. Absent means "the latest at bind time"; the binder is expected to
    /// resolve it to a concrete version and record that here, so a later upload cannot silently
    /// change what a serving stub returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// Where the lookup key comes from — the same extraction a [`LookupBehavior`] uses, so the
    /// compiled form carries this across unchanged.
    pub key: LookupKey,
    /// The dataset column matched against the key. Distinct from a CSV path's `keyColumn` only in
    /// that a dataset *declares* its key columns, so a binder can reject an unknown one up front.
    #[serde(rename = "keyColumn")]
    pub key_column: String,
    /// The token replaced in the response, e.g. `${row}`.
    pub into: String,
    /// The digest of the exact bytes this binding resolved to.
    ///
    /// Written by the binder when it pins `version`, not by the author of the config. It is the
    /// pin: two nodes compiling the same block reach the same file because they agree on this,
    /// not because they independently re-resolve a name whose latest version may have moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// CSV data cache for performance
pub struct CsvCache {
    data: RwLock<HashMap<String, Arc<CsvData>>>,
}

impl Default for CsvCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CsvCache {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
        }
    }

    /// Get or load CSV data
    pub fn get_or_load(&self, path: &str, delimiter: char) -> Option<Arc<CsvData>> {
        // Check cache first
        {
            let cache = self.data.read();
            if let Some(data) = cache.get(path) {
                return Some(Arc::clone(data));
            }
        }

        // Load from file. A failure here means a misconfigured data source
        // (missing/unreadable/malformed CSV); surface it instead of silently
        // serving the response with the lookup tokens left unreplaced.
        let data = match CsvData::load(path, delimiter) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!("lookup behavior: failed to load CSV data source '{path}': {e}");
                return None;
            }
        };
        let data = Arc::new(data);

        // Cache it
        {
            let mut cache = self.data.write();
            cache.insert(path.to_string(), Arc::clone(&data));
        }

        Some(data)
    }

    /// Clear cache
    pub fn clear(&self) {
        self.data.write().clear();
    }
}

/// Parsed CSV data
pub struct CsvData {
    /// Column headers
    headers: Vec<String>,
    /// Data rows in file order: a lookup serves the first matching row, as Mountebank's
    /// `selectRowFromCSV` does (issue #1384).
    rows: Vec<Vec<String>>,
    /// First-column value → index of the first row holding it, so the common first-column
    /// lookup stays O(1).
    first_column: HashMap<String, usize>,
}

impl CsvData {
    /// Load CSV from file
    pub fn load<P: AsRef<Path>>(path: P, delimiter: char) -> Result<Self, std::io::Error> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();

        // Parse header row
        let header_line = lines
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "Empty CSV"))??;
        let headers: Vec<String> = header_line
            .split(delimiter)
            .map(|s| s.trim().to_string())
            .collect();

        // Parse data rows
        let mut rows = Vec::new();
        let mut first_column = HashMap::new();
        for line in lines {
            let line = line?;
            let values: Vec<String> = line
                .split(delimiter)
                .map(|s| s.trim().to_string())
                .collect();
            // `split` always yields at least one item, so every line is a row.
            first_column.entry(values[0].clone()).or_insert(rows.len());
            rows.push(values);
        }

        Ok(Self {
            headers,
            rows,
            first_column,
        })
    }

    /// Look up the first row, in file order, whose `key_column` cell equals `key`, and return its
    /// cells as `[column_name]` token replacements. Empty when no row matches or `key_column` is
    /// not a header.
    pub fn lookup(&self, key: &str, key_column: &str) -> HashMap<String, String> {
        let Some(key_idx) = self.headers.iter().position(|h| h == key_column) else {
            return HashMap::new();
        };
        let row = if key_idx == 0 {
            self.first_column.get(key).map(|&i| &self.rows[i])
        } else {
            self.rows
                .iter()
                .find(|values| values.get(key_idx).is_some_and(|v| v == key))
        };
        let Some(values) = row else {
            return HashMap::new();
        };
        self.headers
            .iter()
            .zip(values)
            .map(|(header, value)| (format!("[{header}]"), value.clone()))
            .collect()
    }
}

/// Apply lookup behaviors to response body. Treats `body` and `headers` as authored text; the
/// serve path uses [`apply_lookup_spliced`] so text an earlier pass inserted is not re-scanned.
pub fn apply_lookup_behaviors(
    body: &str,
    headers: &mut HashMap<String, Vec<String>>,
    behaviors: &[LookupBehavior],
    request: &RequestContext,
    csv_cache: &CsvCache,
    stub: crate::imposter::headers::StubRef<'_>,
) -> String {
    let mut spliced_body = Spliced::authored(body.to_string());
    let mut spliced_headers = authored_headers(std::mem::take(headers));
    apply_lookup_spliced(
        &mut spliced_body,
        &mut spliced_headers,
        behaviors,
        request,
        csv_cache,
        stub,
    );
    *headers = plain_headers(spliced_headers);
    spliced_body.into_text()
}

/// Apply lookup behaviors, replacing only tokens that are not wholly inside text an earlier pass
/// inserted (issue #1203): request text a `${request.*}`, `{{ }}` or `copy` pass spliced in is
/// never read as a lookup token, so a client cannot choose which column of the row is served.
///
/// Each cell is recorded as inserted too, so a cell holding another column's token is served as
/// written — regardless of the order the row's columns are visited in.
pub(crate) fn apply_lookup_spliced(
    body: &mut Spliced,
    headers: &mut SplicedHeaders,
    behaviors: &[LookupBehavior],
    request: &RequestContext,
    csv_cache: &CsvCache,
    stub: crate::imposter::headers::StubRef<'_>,
) {
    for behavior in behaviors {
        // Extract key from request
        let key_value = behavior.key.extract(request);

        if let Some(key) = key_value {
            // Load CSV data
            if let Some(csv_data) = csv_cache.get_or_load(
                &behavior.from_data_source.csv.path,
                behavior.from_data_source.csv.delimiter,
            ) {
                // Lookup row
                let replacements = csv_data.lookup(&key, &behavior.from_data_source.csv.key_column);

                // Mountebank (behaviors.js replaceObjectValuesIn) accepts the column as
                // `["col"]`, `['col']` or `[col]`; `lookup` keys each cell by the unquoted form.
                for (column_token, value) in replacements {
                    let column = column_token
                        .strip_prefix('[')
                        .and_then(|c| c.strip_suffix(']'))
                        .unwrap_or(&column_token);
                    let into = &behavior.into;
                    for full_token in [
                        format!("{into}[\"{column}\"]"),
                        format!("{into}['{column}']"),
                        format!("{into}[{column}]"),
                    ] {
                        replace_lookup_token(body, headers, &full_token, &value, stub);
                    }
                }
            }
        }
    }
}

/// Replace one lookup token with a CSV cell in the body and every header value.
fn replace_lookup_token(
    body: &mut Spliced,
    headers: &mut SplicedHeaders,
    full_token: &str,
    value: &str,
    stub: crate::imposter::headers::StubRef<'_>,
) {
    body.replace_token(full_token, value);
    // Per value, so multi-value headers keep their multiplicity (RFC 7230 §3.2.2 forbids folding
    // Set-Cookie).
    //
    // Which CSV cell lands here is chosen by a client-controlled key, so a cell holding a byte a
    // header value cannot carry is a client-selectable 500; repair it like the templating passes
    // do (issue #1067). Only the substituted cell is repaired, so a control character the author
    // typed into the header itself still reaches the builder and fails the response.
    //
    // Skipped outright when no header uses this token — otherwise every CSV column of the matched
    // row would run the repair, and warn, on every request even for a body-only stub.
    if headers
        .values()
        .flatten()
        .any(|v| v.contains_authored(full_token))
    {
        let repaired = crate::imposter::headers::sanitize_header_value(value, stub);
        for header_value in headers.values_mut().flatten() {
            header_value.replace_token(full_token, &repaired);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture identity for tests that only care about the repair itself, not which stub
    /// triggered it — the gate test below (issue #1075) asserts on deliberately chosen values.
    const FIXTURE_STUB: crate::imposter::headers::StubRef<'static> =
        crate::imposter::headers::StubRef {
            port: 0,
            index: 0,
            id: None,
        };

    /// Issue #1067: the client picks the row, so a CSV cell holding a byte a header value cannot
    /// carry is a client-selectable 500. Repair it instead.
    #[test]
    fn lookup_repairs_a_header_value_it_rewrote() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1067_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,Wor\u{1b}ld\n").expect("write csv");

        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ',',
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Echo".to_string(), vec!["n=${row}[name]".to_string()]);

        apply_lookup_behaviors(
            "",
            &mut headers,
            &behaviors,
            &request,
            &CsvCache::default(),
            FIXTURE_STUB,
        );

        assert_eq!(
            headers["X-Echo"],
            vec!["n=World".to_string()],
            "the ESC in the CSV cell is removed; without the repair this value is unrepresentable"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Issue #1075: the repair `lookup` triggers must name the stub too.
    #[test]
    fn lookup_repair_warning_names_the_stub() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1075_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,Wor\u{1b}ld\n").expect("write csv");

        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ',',
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Echo".to_string(), vec!["n=${row}[name]".to_string()]);

        // See the note in `copy.rs`'s sibling test for why this is `captured_logs` and not
        // `tracing_test::traced_test`.
        let events = crate::test_support::captured_logs(|| {
            apply_lookup_behaviors(
                "",
                &mut headers,
                &behaviors,
                &request,
                &CsvCache::default(),
                crate::imposter::headers::StubRef {
                    port: 4646,
                    index: 5,
                    id: None,
                },
            );
        });

        assert_eq!(headers["X-Echo"], vec!["n=World".to_string()]);
        assert_eq!(events.len(), 1, "one repair, one warning");
        assert_eq!(events[0].field("port"), Some("4646"));
        assert_eq!(events[0].field("stub"), Some("5"));
        assert_eq!(
            events[0].field("stub_id"),
            Some(""),
            "no id -> empty, not absent"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Only the substituted cell is repaired; a literal control character the author typed beside
    /// the token survives so the response still fails loudly (issue #1067).
    #[test]
    fn lookup_repairs_the_cell_but_not_a_literal_control_char_beside_the_token() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1067c_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,Wor\u{1b}ld\n").expect("write csv");

        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ',',
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "X-Echo".to_string(),
            vec!["bad\nvalue=${row}[name]".to_string()],
        );

        apply_lookup_behaviors(
            "",
            &mut headers,
            &behaviors,
            &request,
            &CsvCache::default(),
            FIXTURE_STUB,
        );

        assert_eq!(
            headers["X-Echo"],
            vec!["bad\nvalue=World".to_string()],
            "the ESC from the CSV cell is gone; the author's literal newline is not"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The boundary (issue #1067): a header value this behavior never rewrote is a literal from
    /// the config and must reach the builder unrepaired.
    #[test]
    fn lookup_leaves_a_header_value_without_the_token_untouched() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1067b_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,World\n").expect("write csv");

        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ',',
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Literal".to_string(), vec!["bad\nvalue".to_string()]);

        apply_lookup_behaviors(
            "",
            &mut headers,
            &behaviors,
            &request,
            &CsvCache::default(),
            FIXTURE_STUB,
        );

        assert_eq!(
            headers["X-Literal"],
            vec!["bad\nvalue".to_string()],
            "a literal config value must reach the builder unrepaired and fail loudly"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lookup_preserves_multi_value_headers_and_substitutes_each() {
        let path = std::env::temp_dir().join(format!("rift_lookup_272_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,World\n").expect("write csv");

        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ',',
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a=1".to_string(), "n=${row}[name]".to_string()],
        );

        apply_lookup_behaviors(
            "",
            &mut headers,
            &behaviors,
            &request,
            &CsvCache::default(),
            FIXTURE_STUB,
        );

        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=1".to_string(), "n=World".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A `_rift.dataset` block survives a parse/serialize round trip.
    ///
    /// This is the whole contract of the type: the cluster stores what the operator wrote, and a
    /// field that silently vanishes on parse means the binding cannot be stored at all. Asserted
    /// on the *parsed* values rather than by string equality, so a reordering of the struct's
    /// fields does not read as a regression.
    #[test]
    fn a_dataset_binding_round_trips() {
        let json = r#"{
            "name": "customers",
            "version": 3,
            "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
            "keyColumn": "customer_id",
            "into": "${row}",
            "digest": "abc123"
        }"#;

        let parsed: DatasetBinding = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.name, "customers");
        assert_eq!(parsed.version, Some(3));
        assert_eq!(parsed.key_column, "customer_id");
        assert_eq!(parsed.into, "${row}");
        assert_eq!(parsed.digest.as_deref(), Some("abc123"));

        let reparsed: DatasetBinding =
            serde_json::from_str(&serde_json::to_string(&parsed).expect("serializes"))
                .expect("re-parses");
        assert_eq!(reparsed.name, parsed.name);
        assert_eq!(reparsed.version, parsed.version);
        assert_eq!(reparsed.key_column, parsed.key_column);
        assert_eq!(reparsed.into, parsed.into);
        assert_eq!(reparsed.digest, parsed.digest);
    }

    /// `version` and `digest` are optional going in, and absent going out.
    ///
    /// An unbound block is what an operator writes ("latest at bind time"); emitting
    /// `"version": null` would make an unresolved binding indistinguishable from one pinned to a
    /// version that failed to parse.
    #[test]
    fn an_unpinned_dataset_binding_omits_version_and_digest() {
        let json = r#"{
            "name": "customers",
            "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
            "keyColumn": "id",
            "into": "${row}"
        }"#;

        let parsed: DatasetBinding = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.version, None);
        assert_eq!(parsed.digest, None);

        let out = serde_json::to_string(&parsed).expect("serializes");
        assert!(
            !out.contains("version"),
            "unpinned block must omit version: {out}"
        );
        assert!(
            !out.contains("digest"),
            "unpinned block must omit digest: {out}"
        );
    }

    // Issue #1240: Mountebank's lookup `key` takes an `index` beside `from`/`using`, choosing which
    // element of the selector's match array keys the row (`lookupRow`: `keyValues[index]`). For a
    // regex that array is `RegExp.exec`'s: the whole match at 0, then each capture group. Rift
    // ignored the field and always keyed on the first group.

    /// Runs one lookup parsed from `key` against `path`/`body` and returns the served body.
    fn lookup_with_key(
        tag: &str,
        key: serde_json::Value,
        path: &str,
        body: Option<&str>,
    ) -> String {
        let csv =
            std::env::temp_dir().join(format!("rift_lookup_1240_{tag}_{}.csv", std::process::id()));
        std::fs::write(
            &csv,
            "id,name\nab,first\ncd,second\n/ab-cd,whole\nb2,second-book\n",
        )
        .expect("write csv");
        let behavior: LookupBehavior = serde_json::from_value(serde_json::json!({
            "key": key,
            "fromDataSource": {"csv": {"path": csv.to_string_lossy(), "keyColumn": "id"}},
            "into": "${row}"
        }))
        .expect("the lookup parses");
        let request = RequestContext {
            method: "GET".to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: body.map(str::to_string),
            ..Default::default()
        };
        let served = apply_lookup_behaviors(
            "${row}[name]",
            &mut HashMap::new(),
            &[behavior],
            &request,
            &CsvCache::default(),
            FIXTURE_STUB,
        );
        let _ = std::fs::remove_file(&csv);
        served
    }

    fn regex_key(index: Option<u64>) -> serde_json::Value {
        let mut key = serde_json::json!({
            "from": "path",
            "using": {"method": "regex", "selector": "^/(ab|cd)-(ab|cd)$"}
        });
        if let Some(i) = index {
            key["index"] = serde_json::json!(i);
        }
        key
    }

    #[test]
    fn a_regex_lookup_index_selects_that_capture_group() {
        assert_eq!(
            lookup_with_key("g2", regex_key(Some(2)), "/ab-cd", None),
            "second"
        );
        assert_eq!(
            lookup_with_key("g1", regex_key(Some(1)), "/ab-cd", None),
            "first"
        );
    }

    #[test]
    fn a_regex_lookup_index_zero_is_the_whole_match() {
        assert_eq!(
            lookup_with_key("g0", regex_key(Some(0)), "/ab-cd", None),
            "whole"
        );
    }

    /// Without `index` the key stays what Rift has always used (the first capture group), so no
    /// existing config changes meaning.
    #[test]
    fn a_lookup_without_index_keeps_the_first_capture_group() {
        assert_eq!(
            lookup_with_key("none", regex_key(None), "/ab-cd", None),
            "first"
        );
    }

    /// An index past the match array keys nothing, as `keyValues[index]` is `undefined` in
    /// Mountebank: no row is found and the token is left for the author to see.
    #[test]
    fn a_lookup_index_past_the_matches_finds_no_row() {
        assert_eq!(
            lookup_with_key("oob", regex_key(Some(3)), "/ab-cd", None),
            "${row}[name]"
        );
    }

    #[test]
    fn a_jsonpath_lookup_index_selects_that_match() {
        let key = serde_json::json!({
            "from": "body",
            "using": {"method": "jsonpath", "selector": "$.books[*].id"},
            "index": 1
        });
        assert_eq!(
            lookup_with_key(
                "jp",
                key,
                "/",
                Some(r#"{"books":[{"id":"ab"},{"id":"b2"}]}"#)
            ),
            "second-book"
        );
    }

    #[test]
    fn a_negative_lookup_index_is_refused() {
        let parsed: Result<LookupKey, _> = serde_json::from_value(serde_json::json!({
            "from": "path",
            "using": {"method": "regex", "selector": ".*"},
            "index": -1
        }));
        assert!(parsed.is_err(), "a negative index cannot address a match");
    }

    /// `_rift.dataset.key` is a [`LookupKey`] and must hand back the `index` the operator wrote.
    #[test]
    fn a_dataset_binding_keeps_the_key_index() {
        let parsed: DatasetBinding = serde_json::from_value(serde_json::json!({
            "name": "d",
            "key": {"from": "path", "using": {"method": "regex", "selector": "/(.*)$"}, "index": 1},
            "keyColumn": "id",
            "into": "${row}"
        }))
        .expect("parses");
        let out = serde_json::to_value(&parsed).expect("serializes");
        assert_eq!(out["key"]["index"], serde_json::json!(1), "{out}");

        let unindexed: DatasetBinding = serde_json::from_value(serde_json::json!({
            "name": "d",
            "key": {"from": "path", "using": {"method": "regex", "selector": "/(.*)$"}},
            "keyColumn": "id",
            "into": "${row}"
        }))
        .expect("parses");
        let out = serde_json::to_value(&unindexed).expect("serializes");
        assert!(
            out["key"].get("index").is_none(),
            "absent stays absent: {out}"
        );
    }

    /// Writes `contents` to a per-process temp CSV named after `tag` and loads it.
    fn csv_data(tag: &str, contents: &str) -> CsvData {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1384_{tag}_{}.csv", std::process::id()));
        std::fs::write(&path, contents).expect("write csv");
        let data = CsvData::load(&path, ',').expect("load csv");
        let _ = std::fs::remove_file(&path);
        data
    }

    /// Issue #1384: Mountebank's `selectRowFromCSV` resolves with the first row whose key
    /// column matches, in file order. Rift used to key rows by first cell in a map, so a
    /// repeated key served the last row.
    #[test]
    fn a_repeated_key_serves_the_first_row_in_file_order() {
        let data = csv_data("repeat", "id,name\n1,alice\n1,carol\n");
        assert_eq!(data.lookup("1", "id")["[name]"], "alice");
    }

    /// Issue #1384: a key column other than the first scans rows in file order and serves the
    /// first match, not whichever row a randomly seeded map yields first.
    #[test]
    fn a_non_first_key_column_serves_the_first_matching_row() {
        let data = csv_data(
            "column",
            "id,name,city\n1,alice,paris\n2,bob,rome\n3,dave,rome\n",
        );
        let rome = data.lookup("rome", "city");
        assert_eq!(rome["[name]"], "bob");
        assert_eq!(rome["[id]"], "2");
        assert_eq!(data.lookup("bob", "name")["[city]"], "rome");
        assert!(data.lookup("oslo", "city").is_empty());
    }

    /// `clear` drops cached files, so the next `get_or_load` reads the file again.
    #[test]
    fn clear_drops_the_cached_file() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1384_clear_{}.csv", std::process::id()));
        let path_str = path.to_string_lossy().into_owned();
        std::fs::write(&path, "id,name\n1,old\n").expect("write csv");
        let cache = CsvCache::default();
        let name = |cache: &CsvCache| {
            cache
                .get_or_load(&path_str, ',')
                .expect("loads")
                .lookup("1", "id")["[name]"]
                .clone()
        };
        assert_eq!(name(&cache), "old");

        std::fs::write(&path, "id,name\n1,new\n").expect("rewrite csv");
        assert_eq!(
            name(&cache),
            "old",
            "the cached file is served until cleared"
        );

        cache.clear();
        assert_eq!(name(&cache), "new", "clear forces a reload");
        let _ = std::fs::remove_file(&path);
    }

    /// Serves `template` in both the body and an `X-Row` header for a lookup keyed by the path
    /// `/ab-cd` (first group `ab` → row `first`).
    fn lookup_quoted(tag: &str, template: &str) -> (String, Vec<String>) {
        let csv =
            std::env::temp_dir().join(format!("rift_lookup_1402_{tag}_{}.csv", std::process::id()));
        std::fs::write(&csv, "id,name\nab,first\n").expect("write csv");
        let behavior: LookupBehavior = serde_json::from_value(serde_json::json!({
            "key": regex_key(None),
            "fromDataSource": {"csv": {"path": csv.to_string_lossy(), "keyColumn": "id"}},
            "into": "${row}"
        }))
        .expect("the lookup parses");
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/ab-cd".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert("X-Row".to_string(), vec![template.to_string()]);
        let served = apply_lookup_behaviors(
            template,
            &mut headers,
            &[behavior],
            &request,
            &CsvCache::default(),
            FIXTURE_STUB,
        );
        let _ = std::fs::remove_file(&csv);
        (served, headers.remove("X-Row").expect("header kept"))
    }

    /// Issue #1402: Mountebank's `replaceObjectValuesIn` replaces `${row}["col"]`,
    /// `${row}['col']` and `${row}[col]`; Rift used to replace only the unquoted form.
    #[test]
    fn a_double_quoted_column_token_is_replaced_in_body_and_header() {
        let (body, header) = lookup_quoted("dq", r#"<${row}["name"]>"#);
        assert_eq!(body, "<first>");
        assert_eq!(header, vec!["<first>".to_string()]);
    }

    #[test]
    fn a_single_quoted_column_token_is_replaced_in_body_and_header() {
        let (body, header) = lookup_quoted("sq", "<${row}['name']>");
        assert_eq!(body, "<first>");
        assert_eq!(header, vec!["<first>".to_string()]);
    }

    #[test]
    fn an_unquoted_column_token_is_still_replaced_in_body_and_header() {
        let (body, header) = lookup_quoted("uq", "<${row}[name]>");
        assert_eq!(body, "<first>");
        assert_eq!(header, vec!["<first>".to_string()]);
    }

    #[test]
    fn all_three_spellings_in_one_template_are_replaced() {
        let (body, header) = lookup_quoted("all", r#"${row}["id"]|${row}['name']|${row}[name]"#);
        assert_eq!(body, "ab|first|first");
        assert_eq!(header, vec!["ab|first|first".to_string()]);
    }
}
