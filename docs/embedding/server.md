---
layout: default
title: Embeddable Server
parent: Embedding & SPI
nav_order: 1
---

# Embeddable Server

The `rift` binary is a thin wrapper around library entry points in `rift-http-proxy`. A Rust host can
run the same server in-process — optionally around its own `ImposterManager` — and bind the admin and
metrics planes to addresses of its choosing.

---

## `ServerBuilder`

`ServerBuilder` composes the standard admin API, imposter listeners, and (optionally) the metrics
server, then serves them.

```rust
use rift_http_proxy::{ServerBuilder, Cli};
use clap::Parser;

// Build from parsed CLI options (same flags as the `rift` binary):
let builder = ServerBuilder::from_cli(Cli::parse());
```

| Method | Signature | Purpose |
|:-------|:----------|:--------|
| `from_cli` | `fn from_cli(cli: Cli) -> Self` | Seed the builder from CLI options (port, host, configfile, datadir, TLS defaults, metrics port, …). |
| `manager` | `fn manager(self, manager: Arc<ImposterManager>) -> Self` | **The embedding seam** — inject a pre-built `ImposterManager` (e.g. one wired with custom SPI backends) instead of letting the builder construct the default one. |
| `run` | `async fn run(self) -> anyhow::Result<()>` | Load configs, bind, and serve **forever** (returns only on error/shutdown). |
| `start` | `async fn start(self) -> anyhow::Result<RunningServer>` | Same, but returns a `RunningServer` handle **once bound** — supports ephemeral (`:0`) ports and programmatic shutdown. |

### `RunningServer`

Returned by `start()`; lets the host discover bound addresses and control lifecycle.

| Method | Signature | Purpose |
|:-------|:----------|:--------|
| `admin_addr` | `fn admin_addr(&self) -> SocketAddr` | The bound admin API address (resolve an ephemeral `:0` to the real port). |
| `metrics_addr` | `fn metrics_addr(&self) -> Option<SocketAddr>` | The bound metrics address, or `None` if metrics weren't started. |
| `join` | `async fn join(self) -> anyhow::Result<()>` | Await the server until it exits. |
| `shutdown` | `async fn shutdown(self)` | Trigger a graceful shutdown. |

```rust
use rift_http_proxy::{ServerBuilder, Cli};
use clap::Parser;

let server = ServerBuilder::from_cli(Cli::parse()).start().await?;
println!("admin listening on {}", server.admin_addr());
// ... run your test suite against server.admin_addr() ...
server.shutdown().await;
```

### Injecting a custom `ImposterManager`

`ImposterManager` (from `rift-mock-core`) is the injection point for every SPI backend (see
[Extension Points]({{ site.baseurl }}/embedding/spi/)). Build it, wire your backends, then hand it to
the server:

```rust
use std::sync::Arc;
use rift_mock_core::imposter::ImposterManager;
use rift_http_proxy::{ServerBuilder, Cli};
use clap::Parser;

let manager = Arc::new(
    ImposterManager::new()
        .with_flow_store_provider(my_flow_store_provider)
        .with_request_journal(my_journal),
);

ServerBuilder::from_cli(Cli::parse())
    .manager(manager)
    .run()
    .await?;
```

---

## Bindable admin & metrics servers

For finer control — running only the admin plane, or binding each plane independently — use the
lower-level bind APIs. Both follow the same pattern: pass `:0` to get an OS-assigned port and read
the actual address back from the returned handle.

### `AdminApiServer`

```rust
use std::sync::Arc;
use rift_http_proxy::admin_api::AdminApiServer;

let running = AdminApiServer::new(addr, manager, api_key)   // api_key: Option<String>
    .with_config_source(config_source)                       // ConfigSource, optional
    .with_allow_injection(true)                              // enable JS inject, optional
    .bind()
    .await?;

println!("admin bound to {}", running.local_addr());
```

| Item | Signature | Purpose |
|:-----|:----------|:--------|
| `AdminApiServer::new` | `fn new(addr: SocketAddr, manager: Arc<ImposterManager>, api_key: Option<String>) -> Self` | Construct the admin server; `api_key` (when `Some`) gates the admin API via the `Authorization` header. |
| `with_config_source` | `fn with_config_source(self, source: ConfigSource) -> Self` | Retain the load source so `POST /admin/reload` can re-read it. |
| `with_allow_injection` | `fn with_allow_injection(self, allow: bool) -> Self` | Enable JavaScript `inject` responses. |
| `bind` | `async fn bind(self) -> anyhow::Result<RunningAdminApi>` | Bind and start serving; returns once bound. |

`RunningAdminApi`: `local_addr(&self) -> SocketAddr`, `shutdown(&self)`, `join(self) -> anyhow::Result<()>`.

`ConfigSource` (from `rift-http-proxy`) is either `File { path, no_parse }` (a single `--configfile`,
with optional EJS preprocessing) or `Dir(PathBuf)` (a `--datadir` of one-imposter-per-file configs).

### Metrics server

```rust
use rift_http_proxy::bind_metrics_server;

let metrics = bind_metrics_server(addr).await?;   // addr may be `:0`
println!("metrics bound to {}", metrics.local_addr());
metrics.shutdown().await;
```

| Function / method | Signature | Purpose |
|:------------------|:----------|:--------|
| `run_metrics_server` | `async fn run_metrics_server(addr: SocketAddr) -> anyhow::Result<()>` | Serve metrics forever on a fixed address. |
| `bind_metrics_server` | `async fn bind_metrics_server(addr: SocketAddr) -> anyhow::Result<RunningMetrics>` | Bind (supports `:0`) and return a handle. |
| `RunningMetrics::local_addr` | `fn local_addr(&self) -> SocketAddr` | The bound metrics address. |
| `RunningMetrics::shutdown` | `async fn shutdown(&self)` | Stop the metrics server. |
| `RunningMetrics::join` | `async fn join(self) -> anyhow::Result<()>` | Await until it exits. |

---

## TLS: install the crypto provider

Before serving any HTTPS imposter from an embedding host, install the default rustls (`ring`) crypto
provider once:

```rust
rift_http_proxy::install_default_crypto_provider();
```

It is idempotent, so calling it more than once is safe. The `rift` binary does this for you; an
embedding host must call it itself if it serves TLS.
