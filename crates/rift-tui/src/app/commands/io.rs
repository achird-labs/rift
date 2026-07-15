//! I/O commands (import/export/clipboard/save) for App

use super::super::*;

impl App {
    /// Export imposter config
    pub async fn export_imposter(&mut self, remove_proxies: bool) {
        let port = match &self.view {
            View::ImposterDetail { port } => *port,
            _ => return,
        };

        self.is_loading = true;
        match self.client.export_imposter(port, remove_proxies).await {
            Ok(json) => {
                let title = if remove_proxies {
                    format!("Exported Stubs (Port :{port}) - [s]ave [c]opy [A]pply [Esc]close")
                } else {
                    format!("Exported Config (Port :{port}) - [s]ave [c]opy [Esc]close")
                };
                self.overlay = Overlay::Export {
                    title,
                    content: json,
                    port: Some(port),
                };
            }
            Err(e) => {
                self.set_status(format!("Failed to export: {e}"), StatusLevel::Error);
            }
        }
        self.is_loading = false;
    }

    /// Copy content to clipboard
    pub fn copy_to_clipboard(&mut self, content: &str) {
        match arboard::Clipboard::new() {
            Ok(mut clipboard) => {
                if let Err(e) = clipboard.set_text(content.to_string()) {
                    self.set_status(format!("Failed to copy: {e}"), StatusLevel::Error);
                } else {
                    self.set_status("Copied to clipboard".to_string(), StatusLevel::Success);
                }
            }
            Err(e) => {
                self.set_status(format!("Clipboard not available: {e}"), StatusLevel::Error);
            }
        }
    }

    /// Paste from clipboard, returning the text if successful
    pub(in super::super) fn paste_from_clipboard(&self) -> Option<String> {
        arboard::Clipboard::new()
            .ok()
            .and_then(|mut cb| cb.get_text().ok())
    }

    /// Show save file dialog
    pub fn show_save_dialog(&mut self, content: String, port: u16) {
        // Generate default filename
        let default_path = dirs::home_dir()
            .map(|h| h.join(format!("imposter-{port}.json")))
            .unwrap_or_else(|| std::path::PathBuf::from(format!("imposter-{port}.json")));

        let path_str = default_path.to_string_lossy().to_string();
        self.input_state.cursor_pos = path_str.len();
        self.input_state.file_path = path_str;
        self.overlay = Overlay::FilePathInput {
            prompt: format!("Save imposter :{port} to file"),
            action: FileAction::SaveExport { content, port },
        };
    }

    /// Expand tilde in path to home directory
    pub(super) fn expand_path(path: &str) -> String {
        if let Some(rest) = path.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest).to_string_lossy().to_string();
            }
        } else if path == "~"
            && let Some(home) = dirs::home_dir()
        {
            return home.to_string_lossy().to_string();
        }
        path.to_string()
    }

    /// Save content to file
    pub async fn save_to_file(&mut self, path: &str, content: &str) {
        let expanded_path = Self::expand_path(path);
        match tokio::fs::write(&expanded_path, content).await {
            Ok(_) => {
                self.set_status(format!("Saved to {expanded_path}"), StatusLevel::Success);
                self.overlay = Overlay::None;
            }
            Err(e) => {
                self.set_status(format!("Failed to save: {e}"), StatusLevel::Error);
            }
        }
    }

    /// Show import file dialog
    pub fn show_import_file_dialog(&mut self) {
        let default_path = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());

        self.input_state.cursor_pos = default_path.len();
        self.input_state.file_path = default_path;
        self.overlay = Overlay::FilePathInput {
            prompt: "Import imposter from JSON file".to_string(),
            action: FileAction::ImportFile,
        };
    }

    /// Show import folder dialog
    pub fn show_import_folder_dialog(&mut self) {
        let default_path = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());

        self.input_state.cursor_pos = default_path.len();
        self.input_state.file_path = default_path;
        self.overlay = Overlay::FilePathInput {
            prompt: "Import imposters from folder (*.json)".to_string(),
            action: FileAction::ImportFolder,
        };
    }

    /// Import imposter from file with validation
    pub async fn import_from_file(&mut self, path: &str) {
        self.is_loading = true;
        let expanded_path = Self::expand_path(path);

        match tokio::fs::read_to_string(&expanded_path).await {
            Ok(content) => {
                // Validate the content before importing
                let report = validate_imposter_json(&content, &expanded_path);

                if report.has_errors() {
                    // Block import on errors - show validation results
                    self.validation_scroll_offset = 0;
                    self.overlay = Overlay::ValidationResult {
                        report,
                        action: ValidationAction::EditorInfo, // Can't proceed with errors
                    };
                    self.is_loading = false;
                    return;
                }

                if report.has_warnings() {
                    // Show warnings but allow proceeding
                    self.validation_scroll_offset = 0;
                    self.overlay = Overlay::ValidationResult {
                        report,
                        action: ValidationAction::ProceedWithImport {
                            path: expanded_path.clone(),
                            content: content.clone(),
                        },
                    };
                    self.is_loading = false;
                    return;
                }

                // No issues - proceed with import
                self.do_import(&content).await;
            }
            Err(e) => {
                self.set_status(format!("Failed to read file: {e}"), StatusLevel::Error);
            }
        }

        self.is_loading = false;
    }

    /// Actually perform the import (called after validation passes or user confirms)
    pub async fn do_import(&mut self, content: &str) {
        match serde_json::from_str::<serde_json::Value>(content) {
            Ok(config) => {
                let url = format!("{}/imposters", self.client.base_url());
                let resp = self.client.client().post(url).json(&config).send().await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        self.set_status("Import successful".to_string(), StatusLevel::Success);
                        self.overlay = Overlay::None;
                        self.refresh().await;
                    }
                    Ok(r) => {
                        let body = r.text().await.unwrap_or_default();
                        self.set_status(format!("Failed to import: {body}"), StatusLevel::Error);
                    }
                    Err(e) => {
                        self.set_status(format!("Failed to import: {e}"), StatusLevel::Error);
                    }
                }
            }
            Err(e) => {
                self.set_status(format!("Invalid JSON: {e}"), StatusLevel::Error);
            }
        }
    }

    /// Import imposters from folder
    pub async fn import_from_folder(&mut self, folder: &str) {
        self.is_loading = true;
        let expanded_folder = Self::expand_path(folder);

        let path = std::path::Path::new(&expanded_folder);
        let is_dir = tokio::fs::metadata(path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if !is_dir {
            self.set_status(
                format!("{expanded_folder} is not a directory"),
                StatusLevel::Error,
            );
            self.is_loading = false;
            return;
        }

        let mut imported = 0;
        let mut failed = 0;

        let mut entries = match tokio::fs::read_dir(path).await {
            Ok(entries) => entries,
            Err(e) => {
                self.set_status(format!("Failed to read folder: {e}"), StatusLevel::Error);
                self.is_loading = false;
                return;
            }
        };

        loop {
            // An unreadable entry must neither truncate the scan nor pass as success: `break`
            // would silently skip every remaining file while still reporting "Imported N",
            // and the pre-#564 `.flatten()` skipped it but left it uncounted. Tokio's
            // `ReadDir` stays valid after an `Err` (the failed entry is already consumed), so
            // continuing advances to the next entry and always terminates.
            // Every failure below records WHICH file and WHY (issue #624). The status line can
            // only carry an aggregate count, so without this the individual failures of a batch
            // import are unrecoverable the moment it is written — which is the whole reason the
            // error log exists.
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    failed += 1;
                    self.push_error(format!("unreadable directory entry: {e}"));
                    continue;
                }
            };
            let file_path = entry.path();
            if !file_path.extension().map(|e| e == "json").unwrap_or(false) {
                continue;
            }
            let content = match tokio::fs::read_to_string(&file_path).await {
                Ok(content) => content,
                Err(e) => {
                    failed += 1;
                    self.push_error(format!("{}: {e}", file_path.display()));
                    continue;
                }
            };
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(config) => {
                    let url = format!("{}/imposters", self.client.base_url());
                    match self.client.client().post(url).json(&config).send().await {
                        Ok(resp) if resp.status().is_success() => imported += 1,
                        Ok(resp) => {
                            failed += 1;
                            let status = resp.status();
                            self.push_error(format!(
                                "{}: server rejected it ({status})",
                                file_path.display()
                            ));
                        }
                        Err(e) => {
                            failed += 1;
                            self.push_error(format!("{}: {e}", file_path.display()));
                        }
                    }
                }
                Err(e) => {
                    failed += 1;
                    self.push_error(format!("{}: invalid JSON ({e})", file_path.display()));
                }
            }
        }

        if failed > 0 {
            self.set_status(
                format!("Imported {imported} imposters, {failed} failed"),
                StatusLevel::Warning,
            );
        } else {
            self.set_status(
                format!("Imported {imported} imposters"),
                StatusLevel::Success,
            );
        }

        self.overlay = Overlay::None;
        self.refresh().await;
        self.is_loading = false;
    }

    /// Show export all dialog
    pub fn show_export_all_dialog(&mut self) {
        let default_path = dirs::home_dir()
            .map(|h| h.join("imposters.json"))
            .unwrap_or_else(|| std::path::PathBuf::from("imposters.json"));

        let path_str = default_path.to_string_lossy().to_string();
        self.input_state.cursor_pos = path_str.len();
        self.input_state.file_path = path_str;
        self.overlay = Overlay::FilePathInput {
            prompt: "Export all imposters to file".to_string(),
            action: FileAction::ExportAll,
        };
    }

    /// Show export to folder dialog
    pub fn show_export_folder_dialog(&mut self) {
        let default_path = dirs::home_dir()
            .map(|h| h.join("imposters"))
            .unwrap_or_else(|| std::path::PathBuf::from("imposters"));

        let path_str = default_path.to_string_lossy().to_string();
        self.input_state.cursor_pos = path_str.len();
        self.input_state.file_path = path_str;
        self.overlay = Overlay::FilePathInput {
            prompt: "Export imposters to folder (one file per imposter)".to_string(),
            action: FileAction::ExportToFolder,
        };
    }

    /// Export all imposters to a single file
    pub async fn export_all_to_file(&mut self, path: &str) {
        self.is_loading = true;
        let expanded_path = Self::expand_path(path);

        match self.client.export_all_imposters().await {
            Ok(json) => match tokio::fs::write(&expanded_path, &json).await {
                Ok(_) => {
                    self.set_status(format!("Exported to {expanded_path}"), StatusLevel::Success);
                    self.overlay = Overlay::None;
                }
                Err(e) => {
                    self.set_status(format!("Failed to write: {e}"), StatusLevel::Error);
                }
            },
            Err(e) => {
                self.set_status(format!("Failed to export: {e}"), StatusLevel::Error);
            }
        }

        self.is_loading = false;
    }

    /// Export imposters to individual files in a folder
    pub async fn export_to_folder(&mut self, folder: &str) {
        self.is_loading = true;
        let expanded_folder = Self::expand_path(folder);

        let path = std::path::Path::new(&expanded_folder);

        // Create folder if it doesn't exist
        if let Err(e) = tokio::fs::create_dir_all(path).await {
            self.set_status(format!("Failed to create folder: {e}"), StatusLevel::Error);
            self.is_loading = false;
            return;
        }

        let mut exported = 0;
        let mut failed = 0;
        let mut last_error = None;

        // Collected rather than pushed inline: the loop holds `&self.imposters`, so recording into
        // the error log (which needs `&mut self`) has to wait until it ends.
        let mut failures: Vec<String> = Vec::new();

        for imp in &self.imposters {
            match self.client.export_imposter(imp.port, false).await {
                Ok(json) => {
                    let filename = if let Some(name) = &imp.name {
                        format!("{}-{}.json", imp.port, name.replace(['/', '\\', ' '], "_"))
                    } else {
                        format!("{}.json", imp.port)
                    };
                    let file_path = path.join(filename);
                    match tokio::fs::write(&file_path, &json).await {
                        Ok(_) => exported += 1,
                        Err(e) => {
                            failed += 1;
                            let detail = format!("failed to write {}: {e}", file_path.display());
                            failures.push(detail.clone());
                            last_error = Some(detail);
                        }
                    }
                }
                Err(e) => {
                    failed += 1;
                    let detail = format!("failed to export port {}: {e}", imp.port);
                    failures.push(detail.clone());
                    last_error = Some(detail);
                }
            }
        }

        for failure in failures {
            self.push_error(failure);
        }

        if failed > 0 {
            let detail = last_error.map(|e| format!(" ({e})")).unwrap_or_default();
            self.set_status(
                format!("Exported {exported} imposters, {failed} failed{detail}"),
                StatusLevel::Warning,
            );
        } else {
            self.set_status(
                format!("Exported {exported} imposters to {folder}"),
                StatusLevel::Success,
            );
        }

        self.overlay = Overlay::None;
        self.is_loading = false;
    }
}
