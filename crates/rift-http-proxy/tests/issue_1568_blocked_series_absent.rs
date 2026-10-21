//! Issue #1568: `rift_blocked_connections_total{listener="admin"|"metrics"}` exists only when a
//! policy can refuse. Without `--local-only` neither series is created, so "not enforced" reads
//! differently from "nothing refused" (`docs/features/metrics.md`).
//!
//! Its own test binary: the registry is process-wide, and any `--local-only` server started earlier
//! in the same process would have materialised both series.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::time::Duration;

#[tokio::test]
async fn without_local_only_the_admin_and_metrics_blocked_series_are_absent() {
    let cli = Cli::try_parse_from(["rift", "--port", "0", "--metrics-port", "0"]).expect("cli");
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");
    let metrics = server.metrics_addr().expect("metrics bound");
    let scrape = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client")
        .get(format!("http://127.0.0.1:{}/metrics", metrics.port()))
        .send()
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("metrics text");
    for listener in ["admin", "metrics"] {
        let series = format!("rift_blocked_connections_total{{listener=\"{listener}\"}}");
        assert!(
            !scrape.contains(&series),
            "{series} must not exist without --local-only"
        );
    }
    server.shutdown().await;
}
