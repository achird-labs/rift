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
        help_line("C (Shift+c)", "View server config"),
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
        help_line("D (Shift+d)", "Duplicate selected stub"),
        help_line("[ / ]", "Move selected stub up / down"),
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
        help_line("D (Shift+d)", "Duplicate stub"),
        help_line("y", "Copy stub as curl command"),
        Line::from(""),
        section_header("EDITOR"),
        Line::from(""),
        help_line("Ctrl+S", "Save changes"),
        help_line("Ctrl+F", "Format JSON"),
        help_line("Ctrl+L", "Show full lint results"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The event handlers this overlay documents, read as source (issue #944).
    ///
    /// The help text and the key handling are two match blocks that have to agree, and nothing
    /// but a reader was checking them — which is how five live bindings (`C`, `[`, `]`, `D`,
    /// `Ctrl+L`) came to be undocumented. Comparing them here turns "someone will notice" into a
    /// failing test.
    const EVENTS_RS: &str = include_str!("../app/events.rs");

    /// A view's handler and the help sections its keys may legitimately be documented under.
    ///
    /// The mapping is one-to-many because the imposter list's keys are split across two headers
    /// for readability, and because `j`/`k` are listed once under NAVIGATION rather than repeated
    /// in every view.
    const HANDLERS: &[(&str, &[&str])] = &[
        (
            "handle_imposter_list_event",
            &[
                "NAVIGATION",
                "IMPOSTER LIST (Main View)",
                "IMPORT/EXPORT (Main View)",
            ],
        ),
        (
            "handle_imposter_detail_event",
            &["NAVIGATION", "IMPOSTER DETAIL VIEW"],
        ),
        (
            "handle_stub_detail_event",
            &["NAVIGATION", "STUB DETAIL VIEW"],
        ),
    ];

    /// The body of `fn <name>`, up to the next method at the same indentation.
    fn handler_body<'a>(source: &'a str, name: &str) -> &'a str {
        let start = source
            .find(&format!("fn {name}"))
            .unwrap_or_else(|| panic!("handler {name} no longer exists in events.rs"));
        let rest = &source[start..];
        // Methods are indented four spaces inside the impl block; the next one ends this body.
        // Both spellings have to terminate it, or a `pub(super)` neighbour would be swallowed and
        // its keys attributed to this handler.
        let end = ["\n    async fn ", "\n    pub(super) async fn ", "\n    fn "]
            .iter()
            .filter_map(|marker| rest[1..].find(marker).map(|i| i + 1))
            .min();
        match end {
            Some(end) => &rest[..end],
            None => rest,
        }
    }

    /// Every `KeyCode::Char('x')` literal in `body`, in source order.
    fn handled_chars(body: &str) -> Vec<char> {
        let mut found = Vec::new();
        let mut rest = body;
        while let Some(i) = rest.find("KeyCode::Char('") {
            let after = &rest[i + "KeyCode::Char('".len()..];
            let mut chars = after.chars();
            if let (Some(c), Some('\'')) = (chars.next(), chars.next())
                && !found.contains(&c)
            {
                found.push(c);
            }
            rest = after;
        }
        found
    }

    /// The plain text of the help overlay, grouped by the section header it appears under.
    fn documented_keys_by_section() -> Vec<(String, Vec<String>)> {
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        for line in build_help_text() {
            let spans = &line.spans;
            // A section header is one bold+underlined span; a help line is a bold key plus a
            // description. Both are produced only by the two helpers below.
            if spans.len() == 1 {
                let text = spans[0].content.trim();
                if !text.is_empty() && text.chars().all(|c| !c.is_lowercase()) {
                    sections.push((text.to_string(), Vec::new()));
                }
            } else if spans.len() == 2
                && let Some((_, keys)) = sections.last_mut()
            {
                keys.push(spans[0].content.trim().to_string());
            }
        }
        sections
    }

    /// Does any documented key string in `sections` cover the character `c`?
    ///
    /// Case-sensitively for letters, so the imposter list's `C` (server config) is not counted as
    /// covered by a lowercase `c` documented elsewhere in the same view — that near-miss is
    /// exactly the shape of one of the five gaps this test was written for.
    fn is_documented(c: char, allowed: &[&str], sections: &[(String, Vec<String>)]) -> bool {
        sections
            .iter()
            .filter(|(header, _)| allowed.contains(&header.as_str()))
            .flat_map(|(_, keys)| keys.iter())
            .any(|key| key.contains(c))
    }

    #[test]
    fn every_handled_key_is_documented_in_the_help_overlay() {
        let sections = documented_keys_by_section();
        let mut undocumented: Vec<String> = Vec::new();

        for (handler, allowed) in HANDLERS {
            let body = handler_body(EVENTS_RS, handler);
            for c in handled_chars(body) {
                if !is_documented(c, allowed, &sections) {
                    undocumented.push(format!(
                        "{handler} handles '{c}', not listed under {allowed:?}"
                    ));
                }
            }
        }

        assert!(
            undocumented.is_empty(),
            "the help overlay omits keys the app actually handles:\n  {}",
            undocumented.join("\n  ")
        );
    }

    /// `Ctrl`-modified editor keys are documented as `Ctrl+X`, so they need their own comparison
    /// rather than the bare-character one above.
    #[test]
    fn every_ctrl_editor_key_is_documented_in_the_help_overlay() {
        let body = handler_body(EVENTS_RS, "handle_editor_event");
        let sections = documented_keys_by_section();
        let editor: Vec<String> = sections
            .iter()
            .find(|(header, _)| header == "EDITOR")
            .expect("the help overlay has an EDITOR section")
            .1
            .clone();

        let mut undocumented = Vec::new();
        for c in handled_chars(body) {
            let expected = format!("Ctrl+{}", c.to_ascii_uppercase());
            if !editor.iter().any(|key| key.contains(&expected)) {
                undocumented.push(expected);
            }
        }

        assert!(
            undocumented.is_empty(),
            "the EDITOR help section omits: {undocumented:?}"
        );
    }
}
