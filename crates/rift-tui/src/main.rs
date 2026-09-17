//! Rift TUI - Interactive Terminal User Interface for Rift
//!
//! # Usage
//!
//! ```bash
//! # Run TUI (connects to localhost:2525 by default)
//! rift-tui
//!
//! # Connect to a different server
//! rift-tui --admin-url http://server:2525
//!
//! # Custom refresh interval
//! rift-tui --refresh-ms 500
//! ```

use clap::Parser;
use rift_tui::App;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "rift-tui")]
#[command(author, version, about = "Interactive TUI for Rift")]
struct Args {
    /// Admin API URL
    #[arg(
        short,
        long,
        default_value = "http://localhost:2525",
        env = "RIFT_ADMIN_URL"
    )]
    admin_url: String,

    /// Refresh interval in milliseconds
    #[arg(short, long, default_value = "1000")]
    refresh_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let refresh_interval = Duration::from_millis(args.refresh_ms);
    let app = App::new(&args.admin_url, refresh_interval).await;

    rift_tui::run(app).await
}
