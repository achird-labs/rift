//! Help overlay with scroll support

use ratatui::{
    Frame,
    layout::Alignment,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

/// Draw the help overlay with scrolling
pub fn draw_overlay(frame: &mut Frame, scroll: u16) -> u16 {
    let area = super::centered_rect(75, 85, frame.area());

    // Clear the background
    frame.render_widget(Clear, area);

    let help_text = build_help_text();
    let total_lines = help_text.len() as u16;
    let visible_height = area.height.saturating_sub(2); // Account for borders
    let max_scroll = total_lines.saturating_sub(visible_height);

    let block = Block::default()
        .title(" Rift TUI Help ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .style(Style::default());

    let paragraph = Paragraph::new(help_text).block(block).scroll((scroll, 0));

    frame.render_widget(paragraph, area);

    // Draw scrollbar if content overflows
    if max_scroll > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state =
            ScrollbarState::new(max_scroll as usize).position(scroll as usize);

        let scrollbar_area = ratatui::layout::Rect {
            x: area.x + area.width - 1,
            y: area.y + 1,
            width: 1,
            height: area.height.saturating_sub(2),
        };

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    max_scroll
}

fn build_help_text() -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        section_header("NAVIGATION"),
        Line::from(""),
        help_line("j / ↓", "Move down in list"),
        help_line("k / ↑", "Move up in list"),
        help_line("Enter", "Select / Drill down"),
        help_line("Esc", "Go back / Close overlay"),
        help_line("q", "Quit (from main view)"),
        help_line("Tab", "Switch focus between panes"),
        help_line("r", "Refresh data"),
        help_line("/", "Search / filter items"),
        help_line(
            "T (Shift+t)",
            "Cycle theme (Default/Dark/Light/Nord/Dracula)",
        ),
        help_line("?", "Toggle this help"),
        help_line("L (Shift+l)", "Show recent errors and warnings"),
        Line::from(""),
        section_header("IMPOSTER LIST (Main View)"),
        Line::from(""),
        help_line("n", "Create new imposter"),
        help_line("p", "Create proxy imposter (for recording)"),
        help_line("d", "Delete selected imposter"),
        help_line("t", "Toggle enable/disable"),
        help_line("m", "View metrics dashboard"),
        Line::from(""),
        section_header("IMPORT/EXPORT (Main View)"),
        Line::from(""),
        help_line("i", "Import imposter from file"),
        help_line("I (Shift+i)", "Import imposters from folder"),
        help_line("e", "Export all imposters to file"),
        help_line("E (Shift+e)", "Export imposters to folder"),
        Line::from(""),
        section_header("IMPOSTER DETAIL VIEW"),
        Line::from(""),
        help_line("a", "Add new stub"),
        help_line("e", "Edit selected stub"),
        help_line("d", "Delete selected stub"),
        help_line("y", "Copy stub as curl command"),
        help_line("c", "Clear recorded requests"),
        help_line("C (Shift+c)", "Clear proxy recordings"),
        help_line("x", "Export stubs (remove proxy responses)"),
        help_line("X (Shift+x)", "Export full config"),
        help_line("A (Shift+a)", "Apply recorded stubs (stop proxying)"),
        help_line("t", "Toggle imposter enable/disable"),
        Line::from(""),
        section_header("STUB DETAIL VIEW"),
        Line::from(""),
        help_line("e", "Edit stub"),
        help_line("d", "Delete stub"),
        help_line("y", "Copy stub as curl command"),
        Line::from(""),
        section_header("EDITOR"),
        Line::from(""),
        help_line("Ctrl+S", "Save changes"),
        help_line("Ctrl+F", "Format JSON"),
        help_line("Ctrl+A", "Select all"),
        help_line("Ctrl+C", "Copy selection"),
        help_line("Ctrl+X", "Cut selection"),
        help_line("Ctrl+V", "Paste from clipboard"),
        help_line("Ctrl+K", "Delete line"),
        help_line("Ctrl+U", "Clear line before cursor"),
        help_line("Shift+Arrows", "Extend selection"),
        help_line("Ctrl+←/→", "Move by word"),
        help_line("Esc", "Cancel editing"),
        Line::from(""),
        section_header("SEARCH MODE"),
        Line::from(""),
        help_line("Enter", "Confirm search and select first match"),
        help_line("Esc", "Cancel search"),
        help_line("Ctrl+U", "Clear search query"),
        help_line("Ctrl+V", "Paste into search"),
        Line::from(""),
        section_header("EXPORT OVERLAY"),
        Line::from(""),
        help_line("s", "Save to file"),
        help_line("c", "Copy to clipboard"),
        help_line("A (Shift+a)", "Apply recorded stubs"),
        help_line("j/k or ↑/↓", "Scroll content"),
        help_line("Esc", "Close"),
        Line::from(""),
        Line::from(Span::styled(
            "  [↑/↓] scroll  [PgUp/PgDn] page  [Esc/?] close",
            Style::default().add_modifier(Modifier::ITALIC),
        )),
        Line::from(""),
    ]
}

fn section_header(title: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {title}"),
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    ))
}

fn help_line(key: &'static str, desc: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {key:<16}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc),
    ])
}
