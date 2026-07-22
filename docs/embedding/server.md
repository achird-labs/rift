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
| `join` | `async fn join(self) -> anyhow::Result<()>` | Await the server until it exits, consuming it. |
| `wait` | `async fn wait(&self) -> anyhow::Result<()>` | Await the server until it exits **without consuming it** — so you can race it against your own shutdown signal. |
| `shutdown` | `async fn shutdown(&self)` | Trigger a graceful shutdown. |

```rust
use rift_http_proxy::{ServerBuilder, Cli};
use clap::Parser;

let server = ServerBuilder::from_cli(Cli::parse()).start().await?;
println!("admin listening on {}", server.admin_addr());
// ... run your test suite against server.admin_addr() ...
server.shutdown().await;
```

#### Racing the server against a shutdown signal

`join` moves the server, so a `select!` arm that wins against it can no longer reach `shutdown`.
`wait` borrows instead, which is what lets a host own the shutdown policy — run its own teardown
between the signal and the server stopping, and still surface an admin-plane failure if the server
dies on its own first:

```rust
tokio::select! {
    result = server.wait() => return result,   // the admin plane exited — surface why
    () = termination_signal() => {}            // asked to stop — fall through
}
// your own teardown here (drain a cluster, deregister from a load balancer, ...)
server.shutdown().await;
```

The accept loop's error is delivered to the **first** caller of `wait`/`join`; later calls return
`Ok(())` (`anyhow::Error` is not `Clone`). `shutdown` takes `&self`, so the server can also be held
in an `Arc` and stopped from another task.

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

`RunningAdminApi`: `local_addr(&self) -> SocketAddr`, `shutdown(&self)`, `join(self) -> anyhow::Result<()>`,
`wait(&self) -> anyhow::Result<()>` (the non-consuming form of `join`, as above).

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

## Bootstrap helpers

`ServerBuilder` composes the *running* server, but a binary also has bootstrap concerns around it:
applying an rcfile's defaults, stopping a server by PID file, and saving a running server's
imposters. These live in `rift_http_proxy::bootstrap` so an alternative binary keeps CLI parity with
`rift` instead of reimplementing them (issue #807).

| Function | Signature | Purpose |
|:---------|:----------|:--------|
| `apply_rcfile_defaults` | `fn apply_rcfile_defaults(cli: &mut Cli, rcfile: &Path) -> anyhow::Result<()>` | Fill CLI fields **still at their clap defaults** from a Mountebank-compatible JSON rcfile. An explicitly-supplied flag always wins; unrecognised keys are warned and ignored. |
| `stop_for_restart` | `fn stop_for_restart(pidfile: &Path) -> anyhow::Result<()>` | `stop_server`, except a missing PID file is a satisfied precondition (nothing to stop) rather than an error — the `restart` semantic. |
| `DEFAULT_PIDFILE` | `pub const DEFAULT_PIDFILE: &str` | The `rift.pid` fallback `stop`/`restart` apply when `--pidfile` is absent. Applied at the dispatch site so a plain start never writes a PID file it wasn't asked to. |
| `stop_server` | `fn stop_server(pidfile: &Path) -> anyhow::Result<()>` | Signal the process named in `pidfile` (SIGTERM on unix, `taskkill /F` on Windows), then remove the file. A stale pidfile (process already gone) is cleaned up as `Ok`; a denied or failed signal is an error and the pidfile is kept. |
| `save_imposters_async` | `async fn save_imposters_async(host: &str, port: u16, savefile: &Path, remove_proxies: bool) -> anyhow::Result<()>` | Fetch `GET /imposters?replayable=true` from a running admin API and write it to `savefile`. The async form — call it from an embedder's own runtime. A non-2xx admin response is an error; nothing is written to `savefile`. |
| `save_imposters` | `fn save_imposters(host: &str, port: u16, savefile: &Path, remove_proxies: bool) -> anyhow::Result<()>` | Blocking wrapper over `save_imposters_async` for the sync `save` subcommand path. |

Supported rcfile keys: `port`, `host`, `logLevel`/`loglevel`, `allowInjection`/`allow_injection`,
`localOnly`/`local_only`, `datadir`, `configfile`.

```rust
use rift_http_proxy::bootstrap;
use rift_http_proxy::server::{Cli, Commands};
use clap::Parser;

let mut cli = Cli::parse();
if let Some(rcfile) = cli.rcfile.clone() {
    bootstrap::apply_rcfile_defaults(&mut cli, &rcfile)?;
}

match &cli.command {
    // `--pidfile` is a single global binding (issue #827): it parses before or after the
    // subcommand, and the stop/restart default lives here, not on the flag.
    Some(Commands::Stop) => return bootstrap::stop_server(&pidfile_or_default(&cli)),
    // `restart` is `stop` followed by the normal start path — but a missing PID file means
    // "nothing to stop", not an error, so it uses the restart-specific seam.
    Some(Commands::Restart) => bootstrap::stop_for_restart(&pidfile_or_default(&cli))?,
    Some(Commands::Save { savefile, remove_proxies }) => {
        return bootstrap::save_imposters(&cli.host, cli.port, savefile, *remove_proxies);
    }
    _ => {}
}
```

The `rift` binary writes `--pidfile` only on the **serving** path (inside `run_mountebank_mode`),
never before subcommand dispatch — otherwise `rift --pidfile p restart` would record its own PID and
then signal itself, and a transient `save`/`healthcheck` would clobber a running server's file
(issue #827). An alternative binary should keep that ordering.

`save_imposters` builds its own tokio runtime and blocks on it (it is a sync subcommand path), so do
not call it from inside a running async runtime — it would panic starting a nested runtime. From
async code call `save_imposters_async` directly instead; it awaits rather than driving its own
runtime, so it is safe on an async worker thread.

---

## TLS: install the crypto provider

Before serving any HTTPS imposter from an embedding host, install the default rustls (`ring`) crypto
provider once:

```rust
rift_http_proxy::install_default_crypto_provider();
```

It is idempotent, so calling it more than once is safe. The `rift` binary does this for you; an
embedding host must call it itself if it serves TLS.
