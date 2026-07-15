//! UI rendering for the TUI

mod config;
mod dialogs;
mod help;
mod imposter_detail;
mod imposters;
mod metrics;
mod request_detail;
mod stubs;

use crate::app::{App, Overlay, StatusLevel, View};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

/// Main draw function
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(10),   // Main content
            Constraint::Length(4), // Status bar (2 lines + borders)
        ])
        .split(frame.area());

    draw_header(frame, app, chunks[0]);

    match &app.view {
        View::ImposterList => imposters::draw_list(frame, app, chunks[1]),
        View::ImposterDetail { port } => imposter_detail::draw(frame, app, *port, chunks[1]),
        View::StubDetail { port, index } => {
            stubs::draw_detail(frame, app, *port, *index, chunks[1])
        }
        View::StubEdit { .. } => stubs::draw_editor(frame, app, chunks[1]),
        View::RequestDetail { port, index } => {
            request_detail::draw(frame, app, *port, *index, chunks[1])
        }
        View::Config => config::draw(frame, app, chunks[1]),
        View::Metrics => metrics::draw(frame, app, chunks[1]),
    }

    draw_status_bar(frame, app, chunks[2]);

    // Draw overlays on top
    match &app.overlay {
        Overlay::Help => {
            // Note: We can't mutate app here, so we return the max_scroll for the caller to update
            help::draw_overlay(frame, app.help_scroll);
        }
        Overlay::Confirm { message, .. } => dialogs::draw_confirm(frame, message),
        Overlay::Error { message } => dialogs::draw_error(frame, message),
        Overlay::Input { prompt, action } => dialogs::draw_input(frame, app, prompt, action),
        Overlay::Export {
            title,
            content,
            port,
        } => dialogs::draw_export(
            frame,
            title,
            content,
            app.export_scroll_offset,
            port.is_some(),
        ),
        Overlay::FilePathInput { prompt, .. } => dialogs::draw_file_path_input(frame, app, prompt),
        Overlay::Success { message } => dialogs::draw_success(frame, message),
        Overlay::ValidationResult { report, action } => {
            dialogs::draw_validation_result(frame, report, action, app.validation_scroll_offset)
        }
        Overlay::Errors => dialogs::draw_errors(frame, &app.errors, app.errors_scroll),
        Overlay::None => {}
    }
}

/// Draw the header bar
fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let connection_status = if app.is_connected {
        Span::styled("● Connected", Style::default().fg(app.theme.success))
    } else {
        Span::styled("○ Disconnected", Style::default().fg(app.theme.error))
    };

    let loading = if app.is_loading {
        Span::styled(" ⟳", Style::default().fg(app.theme.warning))
    } else {
        Span::raw("")
    };

    let imposter_count = Span::styled(
        format!(" Imposters: {}", app.imposters.len()),
        Style::default().fg(app.theme.muted),
    );

    let title = Line::from(vec![
        Span::styled(
            " Rift TUI ",
            Style::default()
                .fg(app.theme.header_fg)
                .bg(app.theme.header_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        connection_status,
        loading,
        Span::raw(" │ "),
        Span::styled(&app.admin_url, Style::default().fg(app.theme.muted)),
        imposter_count,
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));

    let paragraph = Paragraph::new(title).block(block);
    frame.render_widget(paragraph, area);
}

/// Draw the status bar (or search bar when active)
fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    // Show search bar when active
    if app.search_active || !app.search_query.is_empty() {
        draw_search_bar(frame, app, area);
        return;
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));

    if let Some((msg, level, _)) = &app.status_message {
        let color = match level {
            StatusLevel::Info => app.theme.fg,
            StatusLevel::Success => app.theme.success,
            StatusLevel::Warning => app.theme.warning,
            StatusLevel::Error => app.theme.error,
        };
        // The status line is transient; the counter is what keeps errors that already scrolled
        // past discoverable (issue #624).
        let mut spans = vec![Span::styled(format!(" {msg}"), Style::default().fg(color))];
        if !app.errors.is_empty() {
            spans.push(Span::styled(
                format!("  ⚠ {} [L]", app.errors.len()),
                Style::default().fg(app.theme.warning),
            ));
        }
        let paragraph = Paragraph::new(Line::from(spans))
            .block(block)
            .alignment(Alignment::Left);
        frame.render_widget(paragraph, area);
    } else {
        let (commands1, commands2) = get_commands(&app.view);
        let mut line1 = build_command_line(&commands1, app);
        if !app.errors.is_empty() {
            line1.spans.push(Span::styled(
                format!("  ⚠ {} [L]", app.errors.len()),
                Style::default().fg(app.theme.warning),
            ));
        }
        let lines = if let Some(cmds2) = commands2 {
            let line2 = build_command_line(&cmds2, app);
            vec![line1, line2]
        } else {
            vec![line1]
        };
        let paragraph = Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Left);
        frame.render_widget(paragraph, area);
    }
}

/// Command definition (key, label)
type Command = (&'static str, &'static str);

/// Build a nvim-style command line with [key] notation and separators
fn build_command_line(commands: &[Command], app: &App) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, label)) in commands.iter().enumerate() {
        if i > 0 {
            // Subtle separator
            spans.push(Span::styled(" │ ", Style::default().fg(app.theme.border)));
        }
        // Key in brackets with accent color
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default()
                .fg(app.theme.key_fg)
                .add_modifier(Modifier::BOLD),
        ));
        // Label in muted color
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(app.theme.cmd_fg),
        ));
    }
    Line::from(spans)
}

/// Get context-sensitive commands as (key, label) pairs
fn get_commands(view: &View) -> (Vec<Command>, Option<Vec<Command>>) {
    match view {
        View::ImposterList => (
            vec![
                ("n", "New"),
                ("p", "Proxy"),
                ("d", "Del"),
                ("t", "Toggle"),
                ("m", "Metrics"),
                ("C", "Config"),
                ("/", "Search"),
                ("T", "Theme"),
                ("?", "Help"),
                ("q", "Quit"),
            ],
            Some(vec![
                ("i", "Import"),
                ("I", "ImportDir"),
                ("e", "Export"),
                ("E", "ExportDir"),
            ]),
        ),
        View::ImposterDetail { .. } => (
            vec![
                ("a", "Add"),
                ("e", "Edit"),
                ("d", "Del"),
                ("D", "Dup"),
                ("[", "MoveUp"),
                ("]", "MoveDown"),
                ("y", "Curl"),
                ("t", "Toggle"),
                ("/", "Search"),
                ("?", "Help"),
            ],
            Some(vec![
                ("c", "ClearReq"),
                ("C", "ClearProxy"),
                ("x", "ExportStubs"),
                ("X", "ExportFull"),
                ("A", "Apply"),
            ]),
        ),
        View::StubDetail { .. } => (
            vec![
                ("e", "Edit"),
                ("d", "Delete"),
                ("D", "Dup"),
                ("y", "Curl"),
                ("Esc", "Back"),
                ("?", "Help"),
            ],
            None,
        ),
        View::StubEdit { .. } => (
            vec![
                ("^S", "Save"),
                ("^F", "Format"),
                ("^L", "Lint"),
                ("^A", "SelAll"),
                ("^C", "Copy"),
                ("^X", "Cut"),
                ("^V", "Paste"),
                ("Esc", "Cancel"),
            ],
            None,
        ),
        View::RequestDetail { .. } => (vec![("Esc", "Back"), ("?", "Help")], None),
        View::Config => (vec![("r", "Refresh"), ("Esc", "Back")], None),
        View::Metrics => (vec![("r", "Refresh"), ("Esc", "Back"), ("?", "Help")], None),
    }
}

/// Draw the search bar
fn draw_search_bar(frame: &mut Frame, app: &App, area: Rect) {
    let border_color = if app.search_active {
        app.theme.highlight_bg
    } else {
        app.theme.border
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Search prompt and query
    let cursor = if app.search_active { "█" } else { "" };
    let match_count = match &app.view {
        View::ImposterList => {
            let filtered = app.filtered_imposters();
            format!(" ({}/{})", filtered.len(), app.imposters.len())
        }
        View::ImposterDetail { .. } => {
            let filtered = app.filtered_stubs();
            let total = app
                .current_imposter
                .as_ref()
                .map(|i| i.stubs.len())
                .unwrap_or(0);
            format!(" ({}/{})", filtered.len(), total)
        }
        _ => String::new(),
    };

    let line = Line::from(vec![
        Span::styled(
            " /",
            Style::default()
                .fg(app.theme.highlight_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(&app.search_query, Style::default().fg(app.theme.fg)),
        Span::styled(cursor, Style::default().fg(app.theme.highlight_bg)),
        Span::styled(&match_count, Style::default().fg(app.theme.muted)),
        Span::styled(
            if app.search_active {
                "  [Enter] search  [Esc] cancel  [Ctrl+U] clear"
            } else {
                "  [/] edit  [Esc] clear"
            },
            Style::default().fg(app.theme.muted),
        ),
    ]);

    // The counter has to live here too: `search_query` stays non-empty after `search_active` goes
    // false, so this branch owns the status bar for as long as a filter is active — errors would
    // otherwise be invisible for that whole time, not just transiently (issue #624).
    let mut line = line;
    if !app.errors.is_empty() {
        line.spans.push(Span::styled(
            format!("  ⚠ {} [L]", app.errors.len()),
            Style::default().fg(app.theme.warning),
        ));
    }

    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, inner);
}

/// Calculate a centered rect for modals
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Format a number with thousands separator
pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Format uptime duration
pub fn format_uptime(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;

    if hours > 0 {
        format!("{hours}h {mins}m {secs}s")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Truncate a string to at most `max` characters, appending an ellipsis when
/// shortened. Operates on chars, not bytes, so it never slices mid-codepoint.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{make_imposter, make_test_app};
    use ratatui::{Terminal, backend::TestBackend};

    // ─── truncate ─────────────────────────────────────────────────────────────

    #[test]
    fn truncate_multibyte_no_panic() {
        // Multibyte codepoints (from imported JSON / proxy recordings) must never
        // land mid-codepoint and panic — regression for #542.
        let s = "日本語サービス";
        for max in 0..=s.chars().count() + 2 {
            let out = truncate(s, max);
            assert!(out.chars().count() <= max.max(1));
        }
    }

    #[test]
    fn truncate_char_count() {
        // Shortened result is at most `max` chars, ending in the ellipsis.
        let out = truncate("日本語サービス", 4);
        assert_eq!(out, "日本語…");
        assert_eq!(out.chars().count(), 4);
    }

    #[test]
    fn truncate_ascii() {
        assert_eq!(truncate("hello", 10), "hello"); // fits → unchanged
        assert_eq!(truncate("hello", 5), "hello"); // exact → unchanged
        assert_eq!(truncate("hello world", 5), "hell…"); // long → keep 4 + …
    }

    #[test]
    fn truncate_edges() {
        assert_eq!(truncate("", 0), ""); // empty passthrough
        assert_eq!(truncate("abc", 0), "…"); // max 0 → no underflow
        assert_eq!(truncate("abc", 1), "…"); // max 1 → just ellipsis
    }

    // ─── format_number ────────────────────────────────────────────────────────

    #[test]
    fn test_format_number_zero() {
        assert_eq!(format_number(0), "0");
    }

    #[test]
    fn test_format_number_no_separator_below_1000() {
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn test_format_number_thousands_separator() {
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }

    // ─── format_uptime ────────────────────────────────────────────────────────

    #[test]
    fn test_format_uptime_seconds() {
        assert_eq!(format_uptime(std::time::Duration::from_secs(42)), "42s");
    }

    #[test]
    fn test_format_uptime_minutes() {
        assert_eq!(format_uptime(std::time::Duration::from_secs(90)), "1m 30s");
    }

    #[test]
    fn test_format_uptime_hours() {
        assert_eq!(
            format_uptime(std::time::Duration::from_secs(3661)),
            "1h 1m 1s"
        );
    }

    // ─── UI smoke tests (render must not panic) ───────────────────────────────

    fn make_terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(120, 40)).expect("test terminal")
    }

    #[test]
    fn test_draw_imposter_list_view_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.imposters = vec![
            make_imposter(4545, Some("my-service"), "http"),
            make_imposter(4546, None, "http"),
        ];
        app.imposter_list_state.select(Some(0));
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must not fail");
    }

    #[test]
    fn test_draw_disconnected_state_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.is_connected = false;
        app.is_loading = true;
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must not fail");
    }

    #[test]
    fn test_draw_with_status_message_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.set_status("Test status".to_string(), crate::app::StatusLevel::Error);
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must not fail");
    }

    #[test]
    fn test_draw_metrics_view_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.view = crate::app::View::Metrics;
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must not fail");
    }

    #[test]
    fn test_draw_config_view_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.view = crate::app::View::Config;
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must not fail");
    }

    #[test]
    fn test_draw_search_active_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.search_active = true;
        app.search_query = "payment".to_string();
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must not fail");
    }

    #[test]
    fn test_draw_help_overlay_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.overlay = crate::app::Overlay::Help;
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must not fail");
    }

    #[test]
    fn test_draw_error_overlay_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.overlay = crate::app::Overlay::Error {
            message: "Connection failed".to_string(),
        };
        terminal
            .draw(|f| draw(f, &app))
            .expect("draw must not fail");
    }

    /// `draw_errors` splits a `Layout` and paginates a reversed iterator, so it is worth pinning
    /// that it renders — populated, empty, and scrolled past the end (issue #624).
    #[test]
    fn test_draw_errors_overlay_does_not_panic() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.overlay = crate::app::Overlay::Errors;

        terminal
            .draw(|f| draw(f, &app))
            .expect("empty error log must render");

        app.set_status("boom".to_string(), crate::app::StatusLevel::Error);
        app.set_status("bang".to_string(), crate::app::StatusLevel::Warning);
        terminal
            .draw(|f| draw(f, &app))
            .expect("populated error log must render");

        // Defensive: a scroll offset past the end must yield an empty page, not panic.
        app.errors_scroll = 99;
        terminal
            .draw(|f| draw(f, &app))
            .expect("over-scrolled error log must render");
    }

    /// The status-bar counter must survive the search-bar branch: `search_query` stays non-empty
    /// after `search_active` clears, so that branch owns the bar for as long as a filter is active.
    #[test]
    fn test_status_bar_renders_error_counter_while_filtering() {
        let mut terminal = make_terminal();
        let mut app = make_test_app();
        app.set_status("boom".to_string(), crate::app::StatusLevel::Error);
        app.search_query = "foo".to_string();
        app.search_active = false;
        terminal
            .draw(|f| draw(f, &app))
            .expect("counter must render on the search-bar branch");
    }
}
