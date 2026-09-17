//! Rift TUI - Interactive Terminal User Interface for Rift
//!
//! This crate provides a full-featured TUI for managing imposters, stubs, and
//! viewing metrics through the Rift Admin API.
//!
//! # Features
//!
//! - **Imposter Management**: Create, delete, enable/disable imposters
//! - **Stub Management**: Add, edit, delete, and reorder stubs
//! - **Proxy Recording**: Create proxy imposters to record API responses
//! - **Metrics Dashboard**: View request counts with sparklines
//! - **Import/Export**: Load and save imposter configurations
//! - **File Explorer**: Built-in file browser for import/export operations
//!
//! # Example
//!
//! ```no_run
//! use rift_tui::App;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let app = App::new("http://localhost:2525", Duration::from_secs(1)).await;
//!     rift_tui::run(app).await
//! }
//! ```

pub mod api;
pub mod app;
pub mod event;
pub mod theme;
pub mod ui;
pub mod validation;

pub use app::App;
pub use event::{Event, EventHandler};
pub use theme::Theme;

use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;

/// Run the TUI application with the given app state.
///
/// This function handles terminal setup, runs the main event loop,
/// and restores the terminal on exit.
pub async fn run(mut app: App) -> anyhow::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run the app
    let result = run_app(&mut terminal, &mut app).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    result
}

/// Main event loop
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    let mut events = EventHandler::new(app.refresh_interval);

    while !app.should_quit {
        // Draw UI
        terminal.draw(|f| ui::draw(f, app))?;

        // Handle events
        if let Some(event) = events.next().await {
            match event {
                Event::Key(key) => {
                    app.handle_key_event(key).await;
                }
                Event::Tick => {
                    // Auto-refresh
                    if app.last_refresh.elapsed() >= app.refresh_interval {
                        app.refresh().await;
                    }
                    app.clear_expired_status();
                }
                Event::Resize(_, _) => {
                    // Terminal will auto-redraw
                }
            }
        }
    }

    Ok(())
}
