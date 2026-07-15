//! Modal dialogs using tui-popup and tui-prompts for a cleaner implementation

use crate::app::{App, ErrorEntry, InputAction, ValidationAction};
use crate::validation::{IssueSeverity, ValidationReport};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::collections::VecDeque;
use tui_popup::Popup;

/// Draw a confirmation dialog with proper sizing for long messages
pub fn draw_confirm(frame: &mut Frame, message: &str) {
    // Calculate size based on message length
    let lines: Vec<&str> = message.lines().collect();
    let max_line_len = lines.iter().map(|l| l.len()).max().unwrap_or(30);
    let width = (max_line_len + 10).clamp(40, 70) as u16;
    let height = (lines.len() + 6).min(15) as u16;

    let area = super::centered_rect(
        (width * 100 / frame.area().width).min(80),
        (height * 100 / frame.area().height).min(50),
        frame.area(),
    );

    // Clear the background
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout for message and buttons
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Top padding
            Constraint::Min(2),    // Message
            Constraint::Length(1), // Spacing
            Constraint::Length(1), // Buttons
            Constraint::Length(1), // Bottom padding
        ])
        .split(inner);

    // Message with wrapping
    let message_paragraph = Paragraph::new(message)
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Center);
    frame.render_widget(message_paragraph, chunks[1]);

    // Buttons
    let buttons = Line::from(vec![
        Span::styled(
            "[Enter]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Confirm   "),
        Span::styled(
            "[Esc]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Cancel"),
    ]);
    let buttons_paragraph = Paragraph::new(buttons).alignment(Alignment::Center);
    frame.render_widget(buttons_paragraph, chunks[3]);
}

/// Draw an error dialog using tui-popup
pub fn draw_error(frame: &mut Frame, message: &str) {
    let content = format!("\n{message}\n\nPress Esc to close");

    let popup = Popup::new(content)
        .title(" Error ")
        .style(Style::default().bg(Color::Black).fg(Color::Red))
        .border_style(Style::default().fg(Color::Red));

    frame.render_widget(popup, frame.area());
}

/// Draw a success message dialog using tui-popup
pub fn draw_success(frame: &mut Frame, message: &str) {
    let content = format!("\n{message}\n\nPress any key to continue");

    let popup = Popup::new(content)
        .title(" Success ")
        .style(Style::default().bg(Color::Black).fg(Color::Green))
        .border_style(Style::default().fg(Color::Green));

    frame.render_widget(popup, frame.area());
}

/// Draw an input dialog for creating imposters
pub fn draw_input(frame: &mut Frame, app: &App, prompt: &str, action: &InputAction) {
    match action {
        InputAction::CreateImposter => draw_create_imposter_input(frame, app, prompt),
        InputAction::CreateProxyImposter => draw_create_proxy_input(frame, app, prompt),
    }
}

/// Helper to create an input field with consistent styling
fn draw_input_field(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    value: &str,
    placeholder: &str,
    focused: bool,
    cursor_pos: Option<usize>,
) {
    let border_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = if focused {
        format!(" ▶ {label} ")
    } else {
        format!("   {label} ")
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Build the text content with cursor if focused
    let text_line = if focused {
        if value.is_empty() {
            Line::from(vec![
                Span::styled(placeholder, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "█",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK),
                ),
            ])
        } else if let Some(pos) = cursor_pos {
            let pos = pos.min(value.len());
            let (before, after) = value.split_at(pos);
            Line::from(vec![
                Span::raw(before),
                Span::styled(
                    "█",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK),
                ),
                Span::raw(after),
            ])
        } else {
            Line::from(vec![
                Span::raw(value),
                Span::styled(
                    "█",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK),
                ),
            ])
        }
    } else if value.is_empty() {
        Line::from(Span::styled(
            placeholder,
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::raw(value))
    };

    let paragraph = Paragraph::new(text_line);
    frame.render_widget(paragraph, inner);
}

/// Draw the regular imposter creation dialog
fn draw_create_imposter_input(frame: &mut Frame, app: &App, prompt: &str) {
    let area = super::centered_rect(55, 45, frame.area());

    // Clear the background
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(" {prompt} "))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout for input fields
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Spacing
            Constraint::Length(3), // Port
            Constraint::Length(3), // Name
            Constraint::Length(3), // Protocol
            Constraint::Length(1), // Spacing
            Constraint::Min(2),    // Help text
        ])
        .split(inner);

    // Port field
    draw_input_field(
        frame,
        chunks[1],
        "Port (optional)",
        &app.input_state.port,
        "auto-assign",
        app.input_state.focus_field == 0,
        None,
    );

    // Name field
    draw_input_field(
        frame,
        chunks[2],
        "Name (optional)",
        &app.input_state.name,
        "unnamed",
        app.input_state.focus_field == 1,
        None,
    );

    // Protocol field
    draw_input_field(
        frame,
        chunks[3],
        "Protocol",
        &app.input_state.protocol,
        "http",
        app.input_state.focus_field == 2,
        None,
    );

    // Help text
    let help = Line::from(vec![
        Span::styled("[Tab]", Style::default().fg(Color::Cyan).bold()),
        Span::raw(" Next  "),
        Span::styled("[Shift+Tab]", Style::default().fg(Color::Cyan).bold()),
        Span::raw(" Prev  "),
        Span::styled("[Enter]", Style::default().fg(Color::Green).bold()),
        Span::raw(" Create  "),
        Span::styled("[Esc]", Style::default().fg(Color::Red).bold()),
        Span::raw(" Cancel"),
    ]);

    let help_paragraph = Paragraph::new(help).alignment(Alignment::Center);
    frame.render_widget(help_paragraph, chunks[5]);
}

/// Draw the proxy imposter creation dialog
fn draw_create_proxy_input(frame: &mut Frame, app: &App, prompt: &str) {
    let area = super::centered_rect(65, 55, frame.area());

    // Clear the background
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(" {prompt} "))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout for input fields
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Spacing
            Constraint::Length(3), // Target URL
            Constraint::Length(3), // Port
            Constraint::Length(3), // Name
            Constraint::Length(3), // Proxy Mode
            Constraint::Length(1), // Spacing
            Constraint::Length(2), // Mode description
            Constraint::Length(1), // Spacing
            Constraint::Min(2),    // Help text
        ])
        .split(inner);

    // Target URL field
    draw_input_field(
        frame,
        chunks[1],
        "Target URL (required)",
        &app.input_state.target_url,
        "https://api.example.com",
        app.input_state.focus_field == 0,
        None,
    );

    // Port field
    draw_input_field(
        frame,
        chunks[2],
        "Port (optional)",
        &app.input_state.port,
        "auto-assign",
        app.input_state.focus_field == 1,
        None,
    );

    // Name field
    draw_input_field(
        frame,
        chunks[3],
        "Name (optional)",
        &app.input_state.name,
        "unnamed",
        app.input_state.focus_field == 2,
        None,
    );

    // Proxy Mode selector
    let mode_focused = app.input_state.focus_field == 3;
    let mode_style = if mode_focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mode_title = if mode_focused {
        " ▶ Proxy Mode (←/→ to change) "
    } else {
        "   Proxy Mode (←/→ to change) "
    };

    let mode_block = Block::default()
        .title(mode_title)
        .borders(Borders::ALL)
        .border_style(mode_style);

    let mode_inner = mode_block.inner(chunks[4]);
    frame.render_widget(mode_block, chunks[4]);

    let mode_text = Line::from(vec![
        Span::styled("◀ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            app.input_state.proxy_mode_str(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ▶", Style::default().fg(Color::DarkGray)),
    ]);

    let mode_paragraph = Paragraph::new(mode_text).alignment(Alignment::Center);
    frame.render_widget(mode_paragraph, mode_inner);

    // Mode description
    let description = match app.input_state.proxy_mode {
        0 => "Record first response, replay subsequent requests",
        1 => "Always forward to backend, keep recording new responses",
        2 => "Always forward to backend, no recording",
        _ => "",
    };
    let desc_paragraph = Paragraph::new(description)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(desc_paragraph, chunks[6]);

    // Help text
    let help = Line::from(vec![
        Span::styled("[Tab]", Style::default().fg(Color::Cyan).bold()),
        Span::raw(" Next  "),
        Span::styled("[←/→]", Style::default().fg(Color::Cyan).bold()),
        Span::raw(" Mode  "),
        Span::styled("[Enter]", Style::default().fg(Color::Green).bold()),
        Span::raw(" Create  "),
        Span::styled("[Esc]", Style::default().fg(Color::Red).bold()),
        Span::raw(" Cancel"),
    ]);

    let help_paragraph = Paragraph::new(help).alignment(Alignment::Center);
    frame.render_widget(help_paragraph, chunks[8]);
}

/// Draw an export overlay showing JSON content
pub fn draw_export(
    frame: &mut Frame,
    title: &str,
    content: &str,
    scroll_offset: u16,
    has_port: bool,
) {
    let area = super::centered_rect(85, 85, frame.area());

    // Clear the background
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(" {title} "))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split for content and help
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // Content
            Constraint::Length(2), // Help
        ])
        .split(inner);

    // Content with scrolling
    let lines: Vec<Line> = content.lines().map(Line::from).collect();
    let total_lines = lines.len() as u16;

    let content_paragraph = Paragraph::new(lines)
        .style(Style::default().fg(Color::White))
        .scroll((scroll_offset, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(content_paragraph, chunks[0]);

    // Scrollbar
    if total_lines > chunks[0].height {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        let mut scrollbar_state =
            ScrollbarState::new(total_lines as usize).position(scroll_offset as usize);
        frame.render_stateful_widget(
            scrollbar,
            chunks[0].inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }

    // Help text - show different options based on context
    let help = if has_port {
        Line::from(vec![
            Span::styled("[s]", Style::default().fg(Color::Green).bold()),
            Span::raw("ave  "),
            Span::styled("[c]", Style::default().fg(Color::Cyan).bold()),
            Span::raw("opy  "),
            Span::styled("[↑/↓]", Style::default().fg(Color::Gray)),
            Span::raw(" Scroll  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red).bold()),
            Span::raw(" Close"),
        ])
    } else {
        Line::from(vec![
            Span::styled("[↑/↓]", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" Scroll  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red).bold()),
            Span::raw(" Close"),
        ])
    };

    let help_paragraph = Paragraph::new(help).alignment(Alignment::Center);
    frame.render_widget(help_paragraph, chunks[1]);
}

/// Draw a file path input dialog
pub fn draw_file_path_input(frame: &mut Frame, app: &App, prompt: &str) {
    let area = super::centered_rect(70, 35, frame.area());

    // Clear the background
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(" {prompt} "))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout for input field and help
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Spacing
            Constraint::Length(3), // File path input
            Constraint::Length(1), // Spacing
            Constraint::Length(1), // Tip text
            Constraint::Length(1), // Spacing
            Constraint::Min(2),    // Help text
        ])
        .split(inner);

    // File path field with cursor at position
    draw_input_field(
        frame,
        chunks[1],
        "Path",
        &app.input_state.file_path,
        "",
        true,
        Some(app.input_state.cursor_pos),
    );

    // Tip text
    let tip = Line::from(Span::styled(
        "Tip: Type the full path or use ~ for home directory",
        Style::default().fg(Color::DarkGray),
    ));
    let tip_paragraph = Paragraph::new(tip).alignment(Alignment::Center);
    frame.render_widget(tip_paragraph, chunks[3]);

    // Help text
    let help = Line::from(vec![
        Span::styled("[←/→]", Style::default().fg(Color::Cyan).bold()),
        Span::raw(" Move  "),
        Span::styled("[Home/End]", Style::default().fg(Color::Cyan).bold()),
        Span::raw(" Jump  "),
        Span::styled("[Enter]", Style::default().fg(Color::Green).bold()),
        Span::raw(" OK  "),
        Span::styled("[Esc]", Style::default().fg(Color::Red).bold()),
        Span::raw(" Cancel"),
    ]);

    let help_paragraph = Paragraph::new(help).alignment(Alignment::Center);
    frame.render_widget(help_paragraph, chunks[5]);
}

/// Draw a validation results popup
pub fn draw_validation_result(
    frame: &mut Frame,
    report: &ValidationReport,
    action: &ValidationAction,
    scroll_offset: u16,
) {
    let area = super::centered_rect(75, 70, frame.area());

    // Clear the background
    frame.render_widget(Clear, area);

    // Determine border color based on severity
    let border_color = if report.has_errors() {
        Color::Red
    } else if report.has_warnings() {
        Color::Yellow
    } else {
        Color::Green
    };

    // Title with summary
    let title = format!(" Validation Results: {} ", report.summary());

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split for issues list and help
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // Issues list
            Constraint::Length(2), // Help
        ])
        .split(inner);

    // Build lines for each issue
    let mut lines: Vec<Line> = Vec::new();

    for issue in &report.issues {
        // Severity indicator
        let (severity_style, severity_icon) = match issue.severity {
            IssueSeverity::Error => (
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                "ERROR",
            ),
            IssueSeverity::Warning => (
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                "WARN ",
            ),
            IssueSeverity::Info => (Style::default().fg(Color::Cyan), "INFO "),
        };

        // Issue header: [E001] Error message
        lines.push(Line::from(vec![
            Span::styled(format!("[{}] ", issue.code), severity_style),
            Span::styled(severity_icon, severity_style),
            Span::raw(" "),
            Span::styled(&issue.message, Style::default().fg(Color::White)),
        ]));

        // Location if available
        if let Some(location) = &issue.location {
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled("at: ", Style::default().fg(Color::DarkGray)),
                Span::styled(location, Style::default().fg(Color::Gray)),
            ]));
        }

        // Suggestion if available
        if let Some(suggestion) = &issue.suggestion {
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled("fix: ", Style::default().fg(Color::Green)),
                Span::styled(suggestion, Style::default().fg(Color::Gray)),
            ]));
        }

        // Blank line between issues
        lines.push(Line::from(""));
    }

    let total_lines = lines.len() as u16;

    let issues_paragraph = Paragraph::new(lines)
        .style(Style::default().fg(Color::White))
        .scroll((scroll_offset, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(issues_paragraph, chunks[0]);

    // Scrollbar if needed
    if total_lines > chunks[0].height {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        let mut scrollbar_state =
            ScrollbarState::new(total_lines as usize).position(scroll_offset as usize);
        frame.render_stateful_widget(
            scrollbar,
            chunks[0].inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }

    // Help text - different based on action
    let help = match action {
        ValidationAction::ProceedWithImport { .. } => Line::from(vec![
            Span::styled("[Enter]", Style::default().fg(Color::Green).bold()),
            Span::raw(" Proceed anyway  "),
            Span::styled("[↑/↓]", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" Scroll  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red).bold()),
            Span::raw(" Cancel"),
        ]),
        ValidationAction::EditorInfo => Line::from(vec![
            Span::styled("[↑/↓]", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" Scroll  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red).bold()),
            Span::raw(" Close"),
        ]),
    };

    let help_paragraph = Paragraph::new(help).alignment(Alignment::Center);
    frame.render_widget(help_paragraph, chunks[1]);
}

/// The in-app error log (issue #624).
///
/// The status line shows one message and expires it, so without this a batch of failures is
/// unrecoverable the moment the line is overwritten. Newest first — the most recent failure is
/// almost always the one being investigated.
pub fn draw_errors(frame: &mut Frame, errors: &VecDeque<ErrorEntry>, scroll_offset: usize) {
    let area = super::centered_rect(75, 70, frame.area());
    frame.render_widget(Clear, area);

    let title = if errors.is_empty() {
        " Errors (none) ".to_string()
    } else {
        format!(" Errors ({}) ", errors.len())
    };

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if errors.is_empty() {
            Color::Green
        } else {
            Color::Red
        }))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let lines: Vec<Line> = if errors.is_empty() {
        vec![Line::from(Span::styled(
            "  No errors recorded.",
            Style::default().fg(Color::Green),
        ))]
    } else {
        errors
            .iter()
            .rev()
            .skip(scroll_offset)
            .map(|e| {
                Line::from(vec![
                    Span::styled(
                        format!("  {} ", e.at.format("%H:%M:%S")),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(e.message.clone(), Style::default().fg(Color::Red)),
                ])
            })
            .collect()
    };

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[0]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            " ↑/↓ scroll · Esc/L close ",
            Style::default().fg(Color::DarkGray),
        ))
        .alignment(Alignment::Center),
        chunks[1],
    );
}
