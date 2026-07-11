//! Lookup behavior - query external data sources.

use super::copy::CopySource;
use super::extraction::ExtractionMethod;
use super::request::RequestContext;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

/// Lookup behavior - query external data source
#[derive(Debug, Clone, Deserialize, Serialize)]
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
pub struct LookupKey {
    /// Request field to extract key from
    pub from: CopySource,
    /// Extraction method
    #[serde(rename = "using")]
    pub extraction: ExtractionMethod,
}

/// External data source configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DataSource {
    /// CSV data source
    pub csv: CsvDataSource,
}

/// CSV data source configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
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
    /// Rows indexed by first column for fast lookup
    rows: HashMap<String, Vec<String>>,
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
        let mut rows = HashMap::new();
        for line in lines {
            let line = line?;
            let values: Vec<String> = line
                .split(delimiter)
                .map(|s| s.trim().to_string())
                .collect();
            if !values.is_empty() {
                rows.insert(values[0].clone(), values);
            }
        }

        Ok(Self { headers, rows })
    }

    /// Lookup a row by key and return column values as token replacements
    pub fn lookup(&self, key: &str, key_column: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();

        // Find key column index
        let key_col_idx = self.headers.iter().position(|h| h == key_column);

        if let Some(key_idx) = key_col_idx {
            // Find row where key column matches
            for (row_key, values) in &self.rows {
                let matches = if key_idx == 0 {
                    row_key == key
                } else {
                    values.get(key_idx).map(|v| v == key).unwrap_or(false)
                };

                if matches {
                    // Return all columns as [column_name] tokens
                    for (i, header) in self.headers.iter().enumerate() {
                        if let Some(value) = values.get(i) {
                            result.insert(format!("[{header}]"), value.clone());
                        }
                    }
                    break;
                }
            }
        }

        result
    }
}

/// Apply lookup behaviors to response body
pub fn apply_lookup_behaviors(
    body: &str,
    headers: &mut HashMap<String, Vec<String>>,
    behaviors: &[LookupBehavior],
    request: &RequestContext,
    csv_cache: &CsvCache,
) -> String {
    let mut result = body.to_string();

    for behavior in behaviors {
        // Extract key from request
        let key_value = behavior
            .key
            .from
            .extract(request)
            .and_then(|v| behavior.key.extraction.extract(&v));

        if let Some(key) = key_value {
            // Load CSV data
            if let Some(csv_data) = csv_cache.get_or_load(
                &behavior.from_data_source.csv.path,
                behavior.from_data_source.csv.delimiter,
            ) {
                // Lookup row
                let replacements = csv_data.lookup(&key, &behavior.from_data_source.csv.key_column);

                // Apply replacements
                for (token, value) in replacements {
                    let full_token = format!("{}{}", behavior.into, token);
                    result = result.replace(&full_token, &value);
                    // Per value, so multi-value headers keep their multiplicity (RFC 7230 §3.2.2
                    // forbids folding Set-Cookie).
                    for header_value in headers.values_mut().flatten() {
                        *header_value = header_value.replace(&full_token, &value);
                    }
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_preserves_multi_value_headers_and_substitutes_each() {
        let path = std::env::temp_dir().join(format!("rift_lookup_272_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,World\n").expect("write csv");

        let mut query = HashMap::new();
        query.insert("q".to_string(), "hi".to_string());
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
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

        apply_lookup_behaviors("", &mut headers, &behaviors, &request, &CsvCache::default());

        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=1".to_string(), "n=World".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }
}
