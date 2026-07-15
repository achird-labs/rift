//! Application state and logic for the TUI

use crate::api::{
    ApiClient, CreateImposterRequest, ImposterDetail, ImposterSummary, MetricsData, Stub,
};
use crate::theme::Theme;
use crate::validation::{ValidationReport, validate_imposter_json, validate_stub_json};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

mod commands;
mod events;
mod search;

/// Maximum number of metrics snapshots to keep for sparklines
const MAX_METRICS_HISTORY: usize = 60;

/// Current view/screen
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    ImposterList,
    ImposterDetail { port: u16 },
    StubDetail { port: u16, index: usize },
    StubEdit { port: u16, index: Option<usize> },
    RequestDetail { port: u16, index: usize },
    Config,
    Metrics,
}

/// Overlay (modal) state
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    None,
    Help,
    Confirm {
        message: String,
        action: PendingAction,
    },
    Error {
        message: String,
    },
    Input {
        prompt: String,
        action: InputAction,
    },
    Export {
        title: String,
        content: String,
        port: Option<u16>, // For save/apply operations
    },
    FilePathInput {
        prompt: String,
        action: FileAction,
    },
    Success {
        message: String,
    },
    ValidationResult {
        report: ValidationReport,
        action: ValidationAction,
    },
    /// The in-app error log (issue #624).
    Errors,
}

/// Actions to take after viewing validation results
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationAction {
    /// Import a file despite warnings
    ProceedWithImport { path: String, content: String },
    /// Editor validation - just informational
    EditorInfo,
}

/// File-related actions
#[derive(Debug, Clone, PartialEq)]
pub enum FileAction {
    SaveExport { content: String, port: u16 },
    ImportFile,
    ImportFolder,
    ExportAll,
    ExportToFolder,
}

/// Actions that need confirmation
#[derive(Debug, Clone, PartialEq)]
pub enum PendingAction {
    DeleteImposter { port: u16 },
    DeleteStub { port: u16, index: usize },
    ClearRequests { port: u16 },
    ClearProxyResponses { port: u16 },
    ApplyRecordedStubs { port: u16 },
}

/// Input actions
#[derive(Debug, Clone, PartialEq)]
pub enum InputAction {
    CreateImposter,
    CreateProxyImposter,
}

/// Status message level
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// How many errors the in-app log keeps. The status line shows one message and expires it after a
/// few seconds, so a batch of failures (a folder import where 12 files fail) leaves nothing behind
/// once the line is overwritten. This is the history (issue #624).
///
/// The TUI cannot log to stderr — it would corrupt the alternate-screen render — and a log file
/// would need a path, rotation, and a way to tell the user where it is. So errors are kept here,
/// where the user already is. Bounded because it is fed by a long-running UI; appending to a
/// VecDeque cannot itself fail, which matters for a channel whose whole job is reporting failure.
pub const MAX_ERROR_ENTRIES: usize = 100;

/// One recorded error or warning, with the wall-clock time it happened so it can be correlated
/// with server-side logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEntry {
    pub at: chrono::DateTime<chrono::Local>,
    pub message: String,
}

/// Metrics history snapshot
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub timestamp: Instant,
    pub total_requests: u64,
    pub per_imposter: HashMap<u16, u64>,
}

/// Parts of a curl request extracted from stub predicates
#[derive(Debug)]
pub(super) struct CurlRequestParts {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub query_params: Vec<(String, String)>,
    pub json_body_parts: Vec<(String, serde_json::Value)>,
    pub raw_body: Option<String>,
}

impl Default for CurlRequestParts {
    fn default() -> Self {
        Self {
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: Vec::new(),
            query_params: Vec::new(),
            json_body_parts: Vec::new(),
            raw_body: None,
        }
    }
}

/// Focus area for split views
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FocusArea {
    Left,
    Right,
}

/// Actions that the editor may request (clipboard operations)
#[derive(Debug, Clone)]
pub enum EditorAction {
    Copy(String),
    Cut(String),
    PasteRequest,
}

/// Stub JSON editor backed by ratatui-textarea
pub struct StubEditor {
    pub editor: ratatui_textarea::TextArea<'static>,
    pub validation_error: Option<String>,
    pub validation_report: Option<ValidationReport>,
    pub original_json: String,
}

impl StubEditor {
    pub fn new(json: &str) -> Self {
        let lines: Vec<String> = json.lines().map(String::from).collect();
        let mut editor = ratatui_textarea::TextArea::new(lines);
        editor.set_line_number_style(
            ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
        );
        editor.set_cursor_line_style(ratatui::style::Style::default());
        editor.set_block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .title(" Edit Stub (Ctrl+S save, Ctrl+F format, Ctrl+L lint, Esc cancel) "),
        );
        let original_json = json.to_string();
        let mut stub_editor = Self {
            editor,
            validation_error: None,
            validation_report: None,
            original_json,
        };
        stub_editor.validate();
        stub_editor
    }

    /// Validate the JSON content using rift-lint
    pub fn validate(&mut self) -> bool {
        let content = self.editor.lines().join("\n");
        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(val) => {
                self.validation_error = None;
                let json_str = serde_json::to_string_pretty(&val).unwrap_or(content);
                let report = validate_stub_json(&json_str);
                if report.has_issues() {
                    self.validation_error = Some(report.summary());
                }
                self.validation_report = Some(report);
                true
            }
            Err(e) => {
                self.validation_error = Some(format!("JSON error: {e}"));
                self.validation_report = None;
                false
            }
        }
    }

    /// Get the stub if valid
    ///
    /// Domain-optional parse: editor content that isn't yet a valid stub is a normal editing
    /// state, not an error — `validate()` is the path that reports the parse error to the user
    /// (issue #611).
    pub fn get_stub(&self) -> Option<crate::api::Stub> {
        let content = self.editor.lines().join("\n");
        serde_json::from_str(&content).ok()
    }

    /// Format the JSON content
    pub fn format(&mut self) {
        let content = self.editor.lines().join("\n");
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
            && let Ok(pretty) = serde_json::to_string_pretty(&val)
        {
            let lines: Vec<String> = pretty.lines().map(String::from).collect();
            self.editor = ratatui_textarea::TextArea::new(lines);
            self.editor.set_line_number_style(
                ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
            );
            self.editor
                .set_cursor_line_style(ratatui::style::Style::default());
            self.editor.set_block(
                ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .title(" Edit Stub (Ctrl+S save, Ctrl+F format, Ctrl+L lint, Esc cancel) "),
            );
        }
    }

    /// Handle a key event. Returns Some(EditorAction) for clipboard operations, None otherwise.
    /// Ctrl+S, Ctrl+F, Ctrl+L must be intercepted by the caller BEFORE calling this.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<EditorAction> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    let yanked = self.editor.yank_text();
                    if !yanked.is_empty() {
                        return Some(EditorAction::Copy(yanked));
                    }
                    return None;
                }
                KeyCode::Char('x') => {
                    let yanked = self.editor.yank_text();
                    if !yanked.is_empty() {
                        self.editor.input(crossterm_key_to_input(key));
                        return Some(EditorAction::Cut(yanked));
                    }
                    return None;
                }
                KeyCode::Char('v') => {
                    return Some(EditorAction::PasteRequest);
                }
                _ => {}
            }
        }
        self.editor.input(crossterm_key_to_input(key));
        None
    }
}

/// Convert a `crossterm::event::KeyEvent` to `ratatui_textarea::Input`.
///
/// ratatui-textarea uses its own re-exported crossterm types which differ from
/// the standalone `crossterm` crate used by the rest of the app.
pub(super) fn crossterm_key_to_input(key: KeyEvent) -> ratatui_textarea::Input {
    use ratatui_textarea::{Input, Key};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let k = match key.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Enter => Key::Enter,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => {
            return Input {
                key: Key::Tab,
                ctrl,
                alt,
                shift: true,
            };
        }
        KeyCode::Delete => Key::Delete,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Esc => Key::Esc,
        KeyCode::F(n) => Key::F(n),
        _ => Key::Null,
    };
    Input {
        key: k,
        ctrl,
        alt,
        shift,
    }
}

/// Input state for dialogs
#[derive(Debug, Clone, Default)]
pub struct InputState {
    pub port: String,
    pub name: String,
    pub protocol: String,
    pub target_url: String,
    pub proxy_mode: usize, // 0=proxyOnce, 1=proxyAlways, 2=proxyTransparent
    pub focus_field: usize,
    pub file_path: String,
    pub cursor_pos: usize, // Cursor position in file_path
}

impl InputState {
    pub fn proxy_mode_str(&self) -> &str {
        match self.proxy_mode {
            0 => "proxyOnce",
            1 => "proxyAlways",
            2 => "proxyTransparent",
            _ => "proxyOnce",
        }
    }

    pub fn proxy_mode_display(&self) -> &str {
        match self.proxy_mode {
            0 => "proxyOnce (record first, replay after)",
            1 => "proxyAlways (always forward, keep recording)",
            2 => "proxyTransparent (always forward, no recording)",
            _ => "proxyOnce",
        }
    }
}

/// Main application state
pub struct App {
    // Navigation
    pub view: View,
    pub view_stack: Vec<View>,
    pub overlay: Overlay,

    // Data
    pub imposters: Vec<ImposterSummary>,
    pub current_imposter: Option<ImposterDetail>,
    pub metrics: MetricsData,
    pub metrics_history: VecDeque<MetricsSnapshot>,

    // UI State
    pub imposter_list_state: ListState,
    pub stub_list_state: ListState,
    pub request_list_state: ListState,
    pub focus: FocusArea,
    pub status_message: Option<(String, StatusLevel, Instant)>,
    /// Bounded history of errors/warnings; the status line only ever shows the latest (issue #624).
    pub errors: VecDeque<ErrorEntry>,
    /// Scroll offset for the errors overlay.
    pub errors_scroll: usize,

    // Search State
    pub search_active: bool,
    pub search_query: String,

    // Edit State
    pub stub_editor: Option<StubEditor>,
    pub input_state: InputState,
    pub export_scroll_offset: u16,
    pub validation_scroll_offset: u16,
    pub help_scroll: u16,
    pub help_max_scroll: u16,

    // Config view
    pub server_config: Option<serde_json::Value>,

    // Connection
    pub client: ApiClient,
    pub admin_url: String,
    pub theme: Theme,

    // Runtime
    pub should_quit: bool,
    pub is_loading: bool,
    pub is_connected: bool,
    pub last_refresh: Instant,
    pub start_time: Instant,
    pub refresh_interval: Duration,
}

impl App {
    /// Create a new App instance
    pub async fn new(admin_url: &str, refresh_interval: Duration) -> Self {
        let client = ApiClient::new(admin_url);

        let mut app = Self {
            view: View::ImposterList,
            view_stack: Vec::new(),
            overlay: Overlay::None,

            imposters: Vec::new(),
            current_imposter: None,
            metrics: MetricsData::default(),
            metrics_history: VecDeque::with_capacity(MAX_METRICS_HISTORY),

            imposter_list_state: ListState::default(),
            stub_list_state: ListState::default(),
            request_list_state: ListState::default(),
            focus: FocusArea::Left,
            status_message: None,
            errors: VecDeque::new(),
            errors_scroll: 0,

            search_active: false,
            search_query: String::new(),

            stub_editor: None,
            input_state: InputState {
                protocol: "http".to_string(),
                ..Default::default()
            },
            export_scroll_offset: 0,
            validation_scroll_offset: 0,
            help_scroll: 0,
            help_max_scroll: 0,

            server_config: None,

            client,
            admin_url: admin_url.to_string(),
            theme: Theme::default(),

            should_quit: false,
            is_loading: false,
            is_connected: false,
            last_refresh: Instant::now(),
            start_time: Instant::now(),
            refresh_interval,
        };

        // Initial data load
        app.refresh().await;
        app
    }

    /// Refresh all data from the API
    pub async fn refresh(&mut self) {
        self.is_loading = true;

        // Check connection
        match self.client.health_check().await {
            Ok(healthy) => {
                self.is_connected = healthy;
            }
            Err(_) => {
                self.is_connected = false;
                self.is_loading = false;
                return;
            }
        }

        // Load imposters
        match self.client.list_imposters().await {
            Ok(imposters) => {
                self.imposters = imposters;
                // Ensure selection is valid
                if !self.imposters.is_empty() {
                    if self.imposter_list_state.selected().is_none() {
                        self.imposter_list_state.select(Some(0));
                    } else if let Some(idx) = self.imposter_list_state.selected()
                        && idx >= self.imposters.len()
                    {
                        self.imposter_list_state
                            .select(Some(self.imposters.len() - 1));
                    }
                }
            }
            Err(e) => {
                self.set_status(format!("Failed to load imposters: {e}"), StatusLevel::Error);
            }
        }

        // Load metrics
        if let Ok(metrics) = self.client.get_metrics().await {
            // Update history
            let snapshot = MetricsSnapshot {
                timestamp: Instant::now(),
                total_requests: metrics.total_requests,
                per_imposter: metrics
                    .per_imposter
                    .iter()
                    .map(|(k, v)| (*k, v.request_count))
                    .collect(),
            };
            self.metrics_history.push_back(snapshot);
            if self.metrics_history.len() > MAX_METRICS_HISTORY {
                self.metrics_history.pop_front();
            }

            self.metrics = metrics;
        }

        // Refresh current imposter if viewing detail
        if let View::ImposterDetail { port } | View::StubDetail { port, .. } = self.view
            && let Ok(detail) = self.client.get_imposter(port).await
        {
            self.current_imposter = Some(detail);
        }

        self.is_loading = false;
        self.last_refresh = Instant::now();
    }

    /// Set a status message
    pub fn set_status(&mut self, message: String, level: StatusLevel) {
        // Record failures before the status line overwrites or expires them. Done here rather than
        // at each call site so every existing one gains history without being touched — and so a
        // future one cannot forget (issue #624). Success/Info are transient by nature and would
        // drown the log.
        if matches!(level, StatusLevel::Error | StatusLevel::Warning) {
            self.push_error(message.clone());
        }
        self.status_message = Some((message, level, Instant::now()));
    }

    /// Append to the bounded error log, dropping the oldest entry at capacity.
    ///
    /// Public so a caller with more detail than the status line can carry — e.g. a folder import
    /// recording every failed file, not just the last — can record it.
    pub fn push_error(&mut self, message: String) {
        // `>=` not `==`: `errors` is a pub field, so nothing structurally guarantees this is
        // the only mutator.
        while self.errors.len() >= MAX_ERROR_ENTRIES {
            self.errors.pop_front();
        }
        self.errors.push_back(ErrorEntry {
            at: chrono::Local::now(),
            message,
        });
    }

    /// Clear status if expired
    pub fn clear_expired_status(&mut self) {
        if let Some((_, _, time)) = &self.status_message
            && time.elapsed() > Duration::from_secs(5)
        {
            self.status_message = None;
        }
    }

    /// Cycle to the next theme
    pub fn cycle_theme(&mut self) {
        self.theme.next();
        self.set_status(
            format!("Theme: {}", self.theme.preset.name()),
            StatusLevel::Info,
        );
    }

    /// Navigate to a new view
    pub fn navigate(&mut self, view: View) {
        self.view_stack.push(self.view.clone());
        self.view = view;
        // Clear search when navigating
        self.search_active = false;
        self.search_query.clear();
    }

    /// Go back to previous view
    pub fn go_back(&mut self) {
        // Clear search when going back
        if self.search_active || !self.search_query.is_empty() {
            self.search_active = false;
            self.search_query.clear();
            return;
        }

        if let Some(prev) = self.view_stack.pop() {
            self.view = prev;
        } else if self.view != View::ImposterList {
            self.view = View::ImposterList;
        } else {
            self.should_quit = true;
        }
    }

    /// Get selected imposter
    pub fn selected_imposter(&self) -> Option<&ImposterSummary> {
        self.imposter_list_state
            .selected()
            .and_then(|i| self.imposters.get(i))
    }

    /// Switch focus between panes
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            FocusArea::Left => FocusArea::Right,
            FocusArea::Right => FocusArea::Left,
        };
    }

    /// Execute a pending action
    pub async fn execute_pending_action(&mut self) {
        if let Overlay::Confirm { action, .. } = &self.overlay.clone() {
            match action {
                PendingAction::DeleteImposter { port } => {
                    self.delete_imposter(*port).await;
                }
                PendingAction::DeleteStub { port, index } => {
                    self.delete_stub(*port, *index).await;
                }
                PendingAction::ClearRequests { port } => {
                    self.clear_requests(*port).await;
                }
                PendingAction::ClearProxyResponses { port } => {
                    self.clear_proxy_responses(*port).await;
                }
                PendingAction::ApplyRecordedStubs { port } => {
                    self.apply_recorded_stubs(*port).await;
                }
            }
        }
    }

    /// Get sparkline data for a specific imposter
    pub fn get_sparkline_data(&self, port: u16) -> Vec<u64> {
        self.metrics_history
            .iter()
            .filter_map(|s| s.per_imposter.get(&port).copied())
            .collect()
    }

    /// Calculate request rate between snapshots
    pub fn calculate_rates(&self) -> HashMap<u16, f64> {
        let mut rates = HashMap::new();

        if self.metrics_history.len() >= 2 {
            let recent: Vec<_> = self.metrics_history.iter().rev().take(2).collect();
            if let (Some(newer), Some(older)) = (recent.first(), recent.get(1)) {
                let time_diff = newer
                    .timestamp
                    .duration_since(older.timestamp)
                    .as_secs_f64();
                if time_diff > 0.0 {
                    for (port, count) in &newer.per_imposter {
                        if let Some(old_count) = older.per_imposter.get(port) {
                            let rate = (*count as f64 - *old_count as f64) / time_diff;
                            rates.insert(*port, rate.max(0.0));
                        }
                    }
                }
            }
        }

        rates
    }
}

#[cfg(test)]
pub(crate) mod tests {
    /// The status line shows one message and expires it after 5s, so a batch of failures leaves
    /// nothing behind: the 2nd..Nth errors of a folder import are unrecoverable once the line is
    /// overwritten. The buffer is the history (issue #624).
    #[test]
    fn set_status_records_errors_and_warnings_but_not_successes() {
        let mut app = make_test_app();

        app.set_status("boom".to_string(), StatusLevel::Error);
        app.set_status("careful".to_string(), StatusLevel::Warning);
        app.set_status("all good".to_string(), StatusLevel::Success);
        app.set_status("fyi".to_string(), StatusLevel::Info);

        let recorded: Vec<&str> = app.errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            recorded,
            vec!["boom", "careful"],
            "only Error/Warning are worth history; Success/Info would drown them"
        );
    }

    #[test]
    fn error_buffer_is_bounded_and_drops_the_oldest() {
        let mut app = make_test_app();
        for i in 0..(MAX_ERROR_ENTRIES + 50) {
            app.set_status(format!("err {i}"), StatusLevel::Error);
        }

        assert_eq!(
            app.errors.len(),
            MAX_ERROR_ENTRIES,
            "buffer must stay bounded"
        );
        assert_eq!(
            app.errors.front().map(|e| e.message.as_str()),
            Some(format!("err {}", 50).as_str()),
            "the oldest entries are the ones dropped"
        );
        assert_eq!(
            app.errors.back().map(|e| e.message.as_str()),
            Some(format!("err {}", MAX_ERROR_ENTRIES + 49).as_str()),
            "the newest entry is retained"
        );
    }

    /// The status line expires; the buffer must not — that is the whole point.
    #[test]
    fn clearing_an_expired_status_leaves_the_error_history_intact() {
        let mut app = make_test_app();
        app.set_status("boom".to_string(), StatusLevel::Error);
        app.status_message = None;
        app.clear_expired_status();
        assert_eq!(
            app.errors.len(),
            1,
            "history outlives the transient status line"
        );
    }

    #[tokio::test]
    async fn l_opens_the_errors_overlay_and_esc_dismisses_it() {
        let mut app = make_test_app();
        app.set_status("boom".to_string(), StatusLevel::Error);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.overlay, Overlay::Errors, "`L` opens the error log");

        app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(app.overlay, Overlay::None, "Esc dismisses it");
    }

    /// The scroll guard is the only nontrivial arithmetic here: relaxing `+ 1 < len` to `<=` would
    /// let the offset run one past the last entry, and nothing else would catch it.
    #[tokio::test]
    async fn errors_overlay_scroll_is_bounded_by_the_entry_count() {
        let mut app = make_test_app();
        for i in 0..3 {
            app.set_status(format!("err {i}"), StatusLevel::Error);
        }
        app.overlay = Overlay::Errors;

        // Press Down far more times than there are entries.
        for _ in 0..10 {
            app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .await;
        }
        assert_eq!(
            app.errors_scroll,
            app.errors.len() - 1,
            "scrolling must stop at the last entry, never past it"
        );

        for _ in 0..10 {
            app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
                .await;
        }
        assert_eq!(app.errors_scroll, 0, "scrolling up saturates at the top");
    }

    /// The global key block runs before the view dispatch and returns, so a global binding shadows
    /// any view-local one with the same key. `e` is view-local (export-all / stub-edit), which is
    /// why the error log is on `L` (issue #624).
    #[tokio::test]
    async fn errors_overlay_key_does_not_shadow_the_view_local_e_binding() {
        let mut app = make_test_app();
        app.handle_key_event(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
            .await;
        assert_ne!(
            app.overlay,
            Overlay::Errors,
            "`e` must still reach its view-local handler, not the error log"
        );
    }

    use super::*;
    use crate::api::{ApiClient, ImposterSummary, MetricsData};
    use crate::theme::Theme;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// Build a minimal App without hitting the network.
    pub(crate) fn make_test_app() -> App {
        App {
            view: View::ImposterList,
            view_stack: Vec::new(),
            overlay: Overlay::None,
            imposters: Vec::new(),
            current_imposter: None,
            metrics: MetricsData::default(),
            metrics_history: VecDeque::new(),
            imposter_list_state: ListState::default(),
            stub_list_state: ListState::default(),
            request_list_state: ListState::default(),
            focus: FocusArea::Left,
            status_message: None,
            errors: VecDeque::new(),
            errors_scroll: 0,
            search_active: false,
            search_query: String::new(),
            stub_editor: None,
            input_state: InputState {
                protocol: "http".to_string(),
                ..Default::default()
            },
            export_scroll_offset: 0,
            validation_scroll_offset: 0,
            help_scroll: 0,
            help_max_scroll: 0,
            server_config: None,
            client: ApiClient::new("http://localhost:2525"),
            admin_url: "http://localhost:2525".to_string(),
            theme: Theme::default(),
            should_quit: false,
            is_loading: false,
            is_connected: false,
            last_refresh: Instant::now(),
            start_time: Instant::now(),
            refresh_interval: Duration::from_secs(5),
        }
    }

    pub(crate) fn make_imposter(port: u16, name: Option<&str>, protocol: &str) -> ImposterSummary {
        ImposterSummary {
            port,
            protocol: protocol.to_string(),
            name: name.map(String::from),
            number_of_requests: 0,
            stub_count: 0,
            enabled: true,
            record_requests: false,
        }
    }

    // ─── Navigation ───────────────────────────────────────────────────────────

    #[test]
    fn test_navigate_pushes_current_view_to_stack() {
        let mut app = make_test_app();
        app.navigate(View::Metrics);
        assert_eq!(app.view, View::Metrics);
        assert_eq!(app.view_stack, vec![View::ImposterList]);
    }

    #[test]
    fn test_navigate_clears_search() {
        let mut app = make_test_app();
        app.search_active = true;
        app.search_query = "foo".to_string();
        app.navigate(View::Metrics);
        assert!(!app.search_active);
        assert!(app.search_query.is_empty());
    }

    #[test]
    fn test_go_back_pops_from_stack() {
        let mut app = make_test_app();
        app.navigate(View::Metrics);
        app.go_back();
        assert_eq!(app.view, View::ImposterList);
        assert!(app.view_stack.is_empty());
    }

    #[test]
    fn test_go_back_with_active_search_clears_search_without_popping() {
        let mut app = make_test_app();
        app.navigate(View::Metrics);
        app.search_active = true;
        app.search_query = "foo".to_string();
        app.go_back();
        // View stays at Metrics (search was cleared, not navigation popped)
        assert_eq!(app.view, View::Metrics);
        assert!(!app.search_active);
        assert!(app.search_query.is_empty());
        // Stack still has ImposterList — not consumed
        assert_eq!(app.view_stack, vec![View::ImposterList]);
    }

    #[test]
    fn test_go_back_from_imposter_list_with_empty_stack_sets_should_quit() {
        let mut app = make_test_app();
        assert!(app.view_stack.is_empty());
        app.go_back();
        assert!(app.should_quit);
    }

    // ─── Focus ────────────────────────────────────────────────────────────────

    #[test]
    fn test_toggle_focus_cycles_left_right() {
        let mut app = make_test_app();
        assert_eq!(app.focus, FocusArea::Left);
        app.toggle_focus();
        assert_eq!(app.focus, FocusArea::Right);
        app.toggle_focus();
        assert_eq!(app.focus, FocusArea::Left);
    }

    // ─── Imposter selection ───────────────────────────────────────────────────

    #[test]
    fn test_selected_imposter_returns_none_when_list_empty() {
        let app = make_test_app();
        assert!(app.selected_imposter().is_none());
    }

    #[test]
    fn test_selected_imposter_returns_correct_entry() {
        let mut app = make_test_app();
        app.imposters = vec![
            make_imposter(4545, None, "http"),
            make_imposter(4546, Some("api"), "http"),
        ];
        app.imposter_list_state.select(Some(1));
        let sel = app.selected_imposter().expect("should have selection");
        assert_eq!(sel.port, 4546);
    }

    // ─── InputState ───────────────────────────────────────────────────────────

    #[test]
    fn test_input_state_proxy_mode_str() {
        let cases = [
            (0, "proxyOnce"),
            (1, "proxyAlways"),
            (2, "proxyTransparent"),
            (99, "proxyOnce"),
        ];
        for (mode, expected) in cases {
            let s = InputState {
                proxy_mode: mode,
                ..Default::default()
            };
            assert_eq!(s.proxy_mode_str(), expected);
        }
    }

    // ─── StubEditor ───────────────────────────────────────────────────────────

    #[test]
    fn test_stub_editor_validates_valid_json() {
        let json = r#"{"predicates":[],"responses":[{"is":{"statusCode":200}}]}"#;
        let editor = StubEditor::new(json);
        assert!(editor.validation_error.is_none());
    }

    #[test]
    fn test_stub_editor_validates_invalid_json() {
        let editor = StubEditor::new("not json at all");
        assert!(editor.validation_error.is_some());
    }

    #[test]
    fn test_stub_editor_get_stub_returns_none_on_invalid_json() {
        let editor = StubEditor::new("{bad json}");
        assert!(editor.get_stub().is_none());
    }

    #[test]
    fn test_stub_editor_get_stub_returns_some_on_valid_json() {
        let json = r#"{"predicates":[],"responses":[]}"#;
        let editor = StubEditor::new(json);
        assert!(editor.get_stub().is_some());
    }

    #[test]
    fn test_stub_editor_format_pretty_prints() {
        let json = r#"{"predicates":[],"responses":[]}"#;
        let mut editor = StubEditor::new(json);
        editor.format();
        let content = editor.editor.lines().join("\n");
        // Pretty-printed JSON should be multi-line
        assert!(content.lines().count() > 1);
    }

    // ─── crossterm_key_to_input ───────────────────────────────────────────────

    #[test]
    fn test_key_to_input_converts_char() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let input = crossterm_key_to_input(key);
        assert!(matches!(input.key, ratatui_textarea::Key::Char('a')));
        assert!(!input.ctrl);
    }

    #[test]
    fn test_key_to_input_ctrl_modifier() {
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let input = crossterm_key_to_input(key);
        assert!(input.ctrl);
    }

    #[test]
    fn test_key_to_input_special_keys() {
        use ratatui_textarea::Key;
        let cases = [
            (KeyCode::Enter, Key::Enter),
            (KeyCode::Backspace, Key::Backspace),
            (KeyCode::Esc, Key::Esc),
            (KeyCode::Home, Key::Home),
            (KeyCode::End, Key::End),
        ];
        for (code, expected) in cases {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            let input = crossterm_key_to_input(key);
            assert_eq!(
                std::mem::discriminant(&input.key),
                std::mem::discriminant(&expected),
                "Key {code:?} should map to {expected:?}"
            );
        }
    }
}
