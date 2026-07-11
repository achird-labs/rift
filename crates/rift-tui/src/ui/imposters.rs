//! Imposter list view

use super::truncate;
use crate::app::App;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
};

/// Draw the imposter list view
pub fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let has_search = !app.search_query.is_empty();

    let items: Vec<ListItem> = app
        .imposters
        .iter()
        .enumerate()
        .map(|(i, imp)| {
            let is_selected = app.imposter_list_state.selected() == Some(i);
            let matches_search = app.imposter_matches_search(imp);

            // Dim non-matching items when searching
            let dim = has_search && !matches_search;

            // Status dot: red when recording, green when enabled, gray when disabled
            let status = if imp.enabled { "●" } else { "○" };
            let status_color = if dim {
                app.theme.muted
            } else if imp.record_requests {
                app.theme.error // Red when recording
            } else if imp.enabled {
                app.theme.enabled
            } else {
                app.theme.disabled
            };

            let name = imp.name.as_deref().unwrap_or("(unnamed)");

            let fg_color = if dim { app.theme.muted } else { app.theme.fg };
            let muted_color = app.theme.muted;

            let line = Line::from(vec![
                Span::styled(
                    if is_selected { " ▶ " } else { "   " },
                    Style::default().fg(if dim {
                        app.theme.muted
                    } else {
                        app.theme.highlight_bg
                    }),
                ),
                Span::styled(format!("{status} "), Style::default().fg(status_color)),
                Span::styled(
                    format!(":{:<5}", imp.port),
                    Style::default().fg(fg_color).add_modifier(if dim {
                        Modifier::empty()
                    } else {
                        Modifier::BOLD
                    }),
                ),
                Span::styled(" │ ", Style::default().fg(app.theme.border)),
                Span::styled(
                    format!("{:<20}", truncate(name, 20)),
                    Style::default().fg(fg_color),
                ),
                Span::styled(" │ ", Style::default().fg(app.theme.border)),
                Span::styled(
                    format!("{:>3} stubs", imp.stub_count),
                    Style::default().fg(muted_color),
                ),
                Span::styled(" │ ", Style::default().fg(app.theme.border)),
                Span::styled(
                    format!("{:>8} reqs", super::format_number(imp.number_of_requests)),
                    Style::default().fg(muted_color),
                ),
            ]);

            ListItem::new(line)
        })
        .collect();

    let title = format!(" Imposters ({}) ", app.imposters.len());

    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.border)),
        )
        .highlight_style(
            Style::default()
                .bg(app.theme.highlight_bg)
                .fg(app.theme.highlight_fg),
        );

    frame.render_stateful_widget(list, area, &mut app.imposter_list_state.clone());

    // Show empty state message
    if app.imposters.is_empty() {
        let msg = if app.is_connected {
            "No imposters. Press [n] to create one, [i] to import, or [p] for a proxy."
        } else {
            "Not connected to Rift. Press [r] to retry, or check that Rift is running."
        };

        let inner = Block::default().borders(Borders::ALL).inner(area);

        let paragraph = ratatui::widgets::Paragraph::new(msg)
            .style(Style::default().fg(app.theme.muted))
            .alignment(ratatui::layout::Alignment::Center);

        // Center vertically
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
