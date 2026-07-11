//! Imposter detail view

use super::truncate;
use crate::app::{App, FocusArea};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

/// Draw the imposter detail view
pub fn draw(frame: &mut Frame, app: &App, port: u16, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // Info panel
            Constraint::Min(10),   // Stubs and right panel
        ])
        .split(area);

    draw_info_panel(frame, app, port, chunks[0]);

    // Split for stubs and right panel (preview + requests)
    let content_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    draw_stubs_panel(frame, app, content_chunks[0]);
    draw_right_panel(frame, app, content_chunks[1]);
}

/// Draw the info panel at the top
fn draw_info_panel(frame: &mut Frame, app: &App, port: u16, area: Rect) {
    // Use current_imposter for details (has enabled field from API)
    let detail = app.current_imposter.as_ref();

    let info = if let Some(imp) = detail {
        let status = if imp.enabled { "Enabled" } else { "Disabled" };
        let status_color = if imp.enabled {
            app.theme.enabled
        } else {
            app.theme.disabled
        };

        let name = imp.name.as_deref().unwrap_or("(unnamed)");

        // Build quick stub summary (show scenario names if available)
        let stub_summary = build_stub_summary(&imp.stubs, area.width.saturating_sub(12) as usize);

        // Recording indicator with red dot
        let recording_spans = if imp.record_requests {
            vec![
                Span::styled("⏺ ", Style::default().fg(app.theme.error)),
                Span::styled("Recording", Style::default().fg(app.theme.error)),
            ]
        } else {
            vec![Span::styled(
                "Recording OFF",
                Style::default().fg(app.theme.muted),
            )]
        };

        vec![
            Line::from(vec![
                Span::styled(" Port: ", Style::default().fg(app.theme.muted)),
                Span::styled(
                    port.to_string(),
                    Style::default()
                        .fg(app.theme.fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  │  Protocol: ", Style::default().fg(app.theme.muted)),
                Span::styled(&imp.protocol, Style::default().fg(app.theme.fg)),
                Span::styled("  │  Status: ", Style::default().fg(app.theme.muted)),
                Span::styled(status, Style::default().fg(status_color)),
            ]),
            Line::from(
                [
                    vec![
                        Span::styled(" Name: ", Style::default().fg(app.theme.muted)),
                        Span::styled(name, Style::default().fg(app.theme.fg)),
                        Span::styled("  │  Requests: ", Style::default().fg(app.theme.muted)),
                        Span::styled(
                            super::format_number(imp.number_of_requests),
                            Style::default().fg(app.theme.fg),
                        ),
                        Span::styled("  │  ", Style::default().fg(app.theme.muted)),
                    ],
                    recording_spans,
                ]
                .concat(),
            ),
            Line::from(vec![
                Span::styled(" Stubs: ", Style::default().fg(app.theme.muted)),
                Span::styled(stub_summary, Style::default().fg(app.theme.fg)),
            ]),
        ]
    } else {
        vec![Line::from(Span::styled(
            "Loading...",
            Style::default().fg(app.theme.muted),
        ))]
    };

    let block = Block::default()
        .title(format!(" Imposter :{port} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));

    let paragraph = Paragraph::new(info).block(block);
    frame.render_widget(paragraph, area);
}

/// Build a quick summary of stubs for the info panel
fn build_stub_summary(stubs: &[crate::api::Stub], max_width: usize) -> String {
    if stubs.is_empty() {
        return "None".to_string();
    }

    // Count stubs with scenario names
    let scenario_count = stubs.iter().filter(|s| s.scenario_name.is_some()).count();
    let proxy_count = stubs
        .iter()
        .filter(|s| {
            s.responses
                .first()
                .map(|r| r.get("proxy").is_some())
                .unwrap_or(false)
        })
        .count();

    let mut summary = format!("{} total", stubs.len());

    if scenario_count > 0 {
        summary.push_str(&format!(", {scenario_count} scenarios"));
    }

    if proxy_count > 0 {
        summary.push_str(&format!(", {proxy_count} proxy"));
    }

    // Add first few scenario names if there's space
    let scenarios: Vec<&str> = stubs
        .iter()
        .filter_map(|s| s.scenario_name.as_deref())
        .take(3)
        .collect();

    if !scenarios.is_empty() {
        let scenarios_str = format!(" ({})", scenarios.join(", "));
        if summary.len() + scenarios_str.len() <= max_width {
            summary.push_str(&scenarios_str);
        }
    }

    truncate(&summary, max_width)
}

/// Draw the stubs panel
fn draw_stubs_panel(frame: &mut Frame, app: &App, area: Rect) {
    let empty_stubs = vec![];
    let stubs = app
        .current_imposter
        .as_ref()
        .map(|i| &i.stubs)
        .unwrap_or(&empty_stubs);

    let is_focused = app.focus == FocusArea::Left;
    let has_search = !app.search_query.is_empty();

    let items: Vec<ListItem> = stubs
        .iter()
        .enumerate()
        .map(|(i, stub)| {
            let is_selected = app.stub_list_state.selected() == Some(i);
            let matches_search = app.stub_matches_search(i);

            // Dim non-matching items when searching
            let dim = has_search && !matches_search;

            // Get stub name - prefer scenario_name, then predicates summary
            let stub_name = if let Some(scenario) = &stub.scenario_name {
                scenario.clone()
            } else if stub.predicates.is_empty() {
                "(default)".to_string()
            } else {
                summarize_predicates(&stub.predicates)
            };

            // Get response type and color
            let (response_type, is_proxy) = if stub.responses.is_empty() {
                ("no response", false)
            } else {
                get_response_type_with_info(&stub.responses[0])
            };

            // Use different colors for different response types
            let response_color = if dim {
                app.theme.muted
            } else if is_proxy {
                ratatui::style::Color::Magenta // Magenta for proxy stubs
            } else {
                app.theme.success
            };

            // Truncate stub name to fit panel width
            let max_name_len = area.width.saturating_sub(15) as usize;
            let display_name = truncate(&stub_name, max_name_len);

            let fg_color = if dim { app.theme.muted } else { app.theme.fg };

            let pred_count = stub.predicates.len();
            let resp_count = stub.responses.len();
            let counts = format!(" {pred_count}p {resp_count}r");

            let line = Line::from(vec![
                Span::styled(
                    if is_selected && is_focused {
                        " ▶ "
                    } else {
                        "   "
                    },
                    Style::default().fg(if dim {
                        app.theme.muted
                    } else {
                        app.theme.highlight_bg
                    }),
                ),
                Span::styled(
                    format!("#{:<2}", i + 1),
                    Style::default().fg(app.theme.muted),
                ),
                Span::styled(format!(" {display_name} "), Style::default().fg(fg_color)),
                Span::styled(
                    format!("[{response_type}]"),
                    Style::default().fg(response_color),
                ),
                Span::styled(counts, Style::default().fg(app.theme.muted)),
            ]);

            ListItem::new(line)
        })
        .collect();

    let border_color = if is_focused {
        app.theme.highlight_bg
    } else {
        app.theme.border
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Stubs ({}) ", stubs.len()))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .highlight_style(
            Style::default()
                .bg(if is_focused {
                    app.theme.highlight_bg
                } else {
                    app.theme.muted
                })
                .fg(app.theme.highlight_fg),
        );

    frame.render_stateful_widget(list, area, &mut app.stub_list_state.clone());
}

/// Draw the right panel with stub preview and recorded requests
fn draw_right_panel(frame: &mut Frame, app: &App, area: Rect) {
    // Split vertically: stub preview on top, requests on bottom
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40), // Stub preview
            Constraint::Percentage(60), // Recorded requests
        ])
        .split(area);

    draw_stub_preview(frame, app, chunks[0]);
    draw_requests_panel(frame, app, chunks[1]);
}

/// Draw a preview of the selected stub
fn draw_stub_preview(frame: &mut Frame, app: &App, area: Rect) {
    let stub = app.current_imposter.as_ref().and_then(|imp| {
        app.stub_list_state
            .selected()
            .and_then(|i| imp.stubs.get(i))
    });

    let block = Block::default()
        .title(" Stub Preview ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(stub) = stub {
        // Format stub JSON with syntax highlighting
        let json = serde_json::to_string_pretty(stub).unwrap_or_default();
        let lines = format_json_preview(&json, inner.height as usize, app);
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    } else {
        let msg = "Select a stub to preview";
        let paragraph = Paragraph::new(msg)
            .style(Style::default().fg(app.theme.muted))
            .alignment(ratatui::layout::Alignment::Center);
        let y_offset = inner.height / 2;
        let centered = Rect {
            x: inner.x,
            y: inner.y + y_offset,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(paragraph, centered);
    }
}

/// Format JSON for preview with basic syntax highlighting
fn format_json_preview<'a>(json: &str, max_lines: usize, app: &App) -> Vec<Line<'a>> {
    json.lines()
        .take(max_lines.saturating_sub(1))
        .map(|line| {
            let mut spans = Vec::new();
            let mut chars = line.chars().peekable();
            let mut current = String::new();
            let mut in_string = false;
            let mut is_key = false;

            while let Some(c) = chars.next() {
                match c {
                    '"' => {
                        if !current.is_empty() {
                            let color = if in_string {
                                if is_key {
                                    ratatui::style::Color::Cyan
                                } else {
                                    ratatui::style::Color::Green
                                }
                            } else {
                                app.theme.fg
                            };
                            spans.push(Span::styled(current.clone(), Style::default().fg(color)));
                            current.clear();
                        }

                        if !in_string {
                            in_string = true;
                            let rest: String = chars.clone().collect();
                            is_key = rest.contains(':');
                        } else {
                            in_string = false;
                        }

                        current.push(c);
                        let color = if is_key {
                            ratatui::style::Color::Cyan
                        } else {
                            ratatui::style::Color::Green
                        };
                        spans.push(Span::styled(current.clone(), Style::default().fg(color)));
                        current.clear();
                    }
                    ':' | ',' | '{' | '}' | '[' | ']' => {
                        if !current.is_empty() {
                            let color = get_json_value_color(&current, app);
                            spans.push(Span::styled(current.clone(), Style::default().fg(color)));
                            current.clear();
                        }
                        spans.push(Span::styled(
                            c.to_string(),
                            Style::default().fg(app.theme.muted),
                        ));
                    }
                    _ => {
                        current.push(c);
                    }
                }
            }

            if !current.is_empty() {
                let color = get_json_value_color(&current, app);
                spans.push(Span::styled(current, Style::default().fg(color)));
            }

            Line::from(spans)
        })
        .collect()
}

/// Get color for JSON values
fn get_json_value_color(s: &str, app: &App) -> ratatui::style::Color {
    let trimmed = s.trim();
    if trimmed == "true" || trimmed == "false" {
        ratatui::style::Color::Yellow
    } else if trimmed == "null" {
        ratatui::style::Color::Red
    } else if trimmed.parse::<f64>().is_ok() {
        ratatui::style::Color::Magenta
    } else {
        app.theme.fg
    }
}

/// Draw the requests panel
fn draw_requests_panel(frame: &mut Frame, app: &App, area: Rect) {
    let empty_requests = vec![];
    let requests = app
        .current_imposter
        .as_ref()
        .map(|i| &i.requests)
        .unwrap_or(&empty_requests);

    let is_focused = app.focus == FocusArea::Right;

    let items: Vec<ListItem> = requests
        .iter()
        .enumerate()
        .map(|(i, req)| {
            let is_selected = app.request_list_state.selected() == Some(i);

            let line = Line::from(vec![
                Span::styled(
                    if is_selected && is_focused {
                        " ▶ "
                    } else {
                        "   "
                    },
                    Style::default().fg(app.theme.highlight_bg),
                ),
                Span::styled(
                    format!("{:<6}", req.method),
                    Style::default().fg(method_color(&req.method, app)),
                ),
                Span::styled(truncate(&req.path, 30), Style::default().fg(app.theme.fg)),
            ]);

            ListItem::new(line)
        })
        .collect();

    let border_color = if is_focused {
        app.theme.highlight_bg
    } else {
        app.theme.border
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Recorded Requests ({}) ", requests.len()))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .highlight_style(
            Style::default()
                .bg(if is_focused {
                    app.theme.highlight_bg
                } else {
                    app.theme.muted
                })
                .fg(app.theme.highlight_fg),
        );

    frame.render_stateful_widget(list, area, &mut app.request_list_state.clone());

    // Show empty state
    if requests.is_empty() {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        let msg = "No requests recorded";
        let paragraph = Paragraph::new(msg)
            .style(Style::default().fg(app.theme.muted))
            .alignment(ratatui::layout::Alignment::Center);

        let y_offset = inner.height / 2;
        let centered = Rect {
            x: inner.x,
            y: inner.y + y_offset,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(paragraph, centered);
    }
}

/// Summarize predicates for display
fn summarize_predicates(predicates: &[serde_json::Value]) -> String {
    if predicates.is_empty() {
        return "(default)".to_string();
    }

    let pred = &predicates[0];
    if let Some(obj) = pred.as_object() {
        for (key, value) in obj {
            match key.as_str() {
                "equals" => {
                    if let Some(v) = value.as_object() {
                        let mut parts = Vec::new();
                        if let Some(m) = v.get("method").and_then(|v| v.as_str()) {
                            parts.push(m.to_string());
                        }
                        if let Some(p) = v.get("path").and_then(|v| v.as_str()) {
                            parts.push(p.to_string());
                        }
                        if !parts.is_empty() {
                            return parts.join(" ");
                        }
                    }
                }
                "contains" | "startsWith" | "endsWith" | "matches" => {
                    return format!("{key} ...");
                }
                "and" | "or" => {
                    if let Some(arr) = value.as_array() {
                        return format!("{} ({} conditions)", key, arr.len());
                    }
                }
                _ => {}
            }
        }
    }

    "(complex)".to_string()
}

/// Get the response type with proxy mode info
fn get_response_type_with_info(response: &serde_json::Value) -> (&str, bool) {
    if response.get("is").is_some() {
        ("is", false)
    } else if let Some(proxy) = response.get("proxy") {
        // Get proxy mode if available
        let mode = proxy
            .get("mode")
            .and_then(|m| m.as_str())
            .unwrap_or("proxy");
        let mode_display = match mode {
            "proxyOnce" => "proxyOnce",
            "proxyAlways" => "proxyAlways",
            "proxyTransparent" => "transparent",
            _ => "proxy",
        };
        (mode_display, true)
    } else if response.get("inject").is_some() {
        ("inject", false)
    } else if response.get("fault").is_some() {
        ("fault", false)
    } else {
        ("unknown", false)
    }
}

/// Get color for HTTP method
fn method_color(method: &str, app: &App) -> ratatui::style::Color {
    match method {
        "GET" => app.theme.success,
        "POST" => ratatui::style::Color::Yellow,
        "PUT" | "PATCH" => ratatui::style::Color::Cyan,
        "DELETE" => app.theme.error,
        _ => app.theme.fg,
    }
}
