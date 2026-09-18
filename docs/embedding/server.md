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

The server composition lives in the `rift_http_proxy::server` module (`Cli`, `Commands`,
`ServerBuilder`, `RunningServer`, `admin_bind_addr`, `run_metrics_server`, `bind_metrics_server`,
`RunningMetrics`); none of these are re-exported at the crate root. The crate root re-exports the
`rift-mock-core` modules plus `TcpFaultKind`, `tcp_fault_carrier`, `default_flow_store_backends` and
`install_default_crypto_provider`.

---

## `ServerBuilder`

`ServerBuilder` composes the standard admin API, imposter listeners, and (optionally) the metrics
server, then serves them.

```rust
use rift_http_proxy::server::{Cli, ServerBuilder};
use clap::Parser;

// Build from parsed CLI options (same flags as the `rift` binary):
let builder = ServerBuilder::from_cli(Cli::parse());
```

| Method | Signature | Purpose |
|:-------|:----------|:--------|
| `from_cli` | `fn from_cli(cli: Cli) -> Self` | Seed the builder from CLI options (port, host, configfile, datadir, TLS defaults, metrics port, …). |
| `manager` | `fn manager(self, manager: Arc<ImposterManager>) -> Self` | **The embedding seam** — inject a pre-built `ImposterManager` (e.g. one wired with custom SPI backends) instead of letting the builder construct the default one. Skips internal construction, including `--datadir` write-through and TLS defaults. |
| `imposter_source` | `fn imposter_source(self, source: Arc<dyn ImposterSource>) -> Self` | Register an `--imposters` URI scheme (repeatable). `file:`/`https:` are built in; claiming a scheme already taken is a startup error. |
| `accept_runtimes` | `fn accept_runtimes(self, runtimes: Vec<tokio::runtime::Handle>) -> Self` | Fan imposter accept loops out across per-core runtimes (issue #745). Applies only to the builder-constructed manager; an injected one uses `ImposterManager::with_accept_runtimes`. Empty keeps the single-listener topology. |
| `admin_authorizer` | `fn admin_authorizer(self, authorizer: Arc<dyn AdminAuthorizer>) -> Self` | Per-request admin authorization hook (issue #854) — see [`AdminAuthorizer`]({{ site.baseurl }}/embedding/spi/#adminauthorizer--per-request-admin-authorization). |
| `reported_admin_port` | `fn reported_admin_port(self, port: u16) -> Self` | The port `GET /config` reports, when the admin plane binds somewhere other than where clients reach it (issue #1135). See `AdminApiServer::with_reported_admin_port` below. |
| `run` | `async fn run(self) -> anyhow::Result<()>` | Load configs, bind, and serve **forever** (returns only on error/shutdown). |
| `start` | `async fn start(self) -> anyhow::Result<RunningServer>` | Same, but returns a `RunningServer` handle **once bound** — supports ephemeral (`:0`) ports and programmatic shutdown. |

### `RunningServer`

Returned by `start()`; lets the host discover bound addresses and control lifecycle.

| Method | Signature | Purpose |
|:-------|:----------|:--------|
| `admin_addr` | `fn admin_addr(&self) -> SocketAddr` | The bound admin API address (resolve an ephemeral `:0` to the real port). |
| `metrics_addr` | `fn metrics_addr(&self) -> Option<SocketAddr>` | The bound metrics address, or `None` if metrics weren't started. |
| `intercept_addr` | `fn intercept_addr(&self) -> Option<SocketAddr>` | The bound [intercept proxy]({{ site.baseurl }}/features/intercept-proxy/) address, whether started by `--intercept-port` or later by `POST /intercept`; `None` when none is running. |
| `front_door_addr` | `fn front_door_addr(&self) -> Option<SocketAddr>` | The bound [front door]({{ site.baseurl }}/features/front-door/) address; `None` unless `--front-door` was given. |
| `join` | `async fn join(self) -> anyhow::Result<()>` | Await the server until it exits, consuming it. |
| `wait` | `async fn wait(&self) -> anyhow::Result<()>` | Await the server until it exits **without consuming it** — so you can race it against your own shutdown signal. |
| `shutdown` | `async fn shutdown(&self)` | Trigger a graceful shutdown. |

```rust
use rift_http_proxy::server::{Cli, ServerBuilder};
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
use rift_http_proxy::server::{Cli, ServerBuilder};
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
| `with_imposter_sources` | `fn with_imposter_sources(self, sources: Arc<SourceSet>, datadir: Option<PathBuf>) -> Self` | Retain an `--imposters` source set, and the `--datadir` loaded beside it, so `POST /admin/reload` re-reads both and applies them as one set. Source imposters are applied as `Persistence::Ephemeral` and never written to the datadir (issue #1122). `ImposterManager::apply_config` keeps a running imposter in the store it is in: it persists what it creates, and changes to imposters already in the datadir. |
| `with_allow_injection` | `fn with_allow_injection(self, allow: bool) -> Self` | Admit scripted configs (`inject`, `decorate`, `shellTransform`, `_rift.script`, …) submitted through this admin API, and report the setting from `GET /config`. The embedder spelling of `--allowInjection`; default `false`. |
| `with_local_only` | `fn with_local_only(self, local_only: bool) -> Self` | Record `--local-only` for `GET /config` to report (issue #879). Stated explicitly, not inferred from the bind address. |
| `with_admin_authorizer` | `fn with_admin_authorizer(self, authorizer: Arc<dyn AdminAuthorizer>) -> Self` | Install the per-request authorization hook (issue #854); `ServerBuilder::admin_authorizer` is the builder spelling. |
| `with_intercept` | `fn with_intercept(self, control: InterceptControl) -> Self` | Serve the `/intercept*` routes against this shared control slot. Without it every `/intercept*` route answers `404`. See [Intercept proxy]({{ site.baseurl }}/features/intercept-proxy/). |
| `with_scripts_dir` | `fn with_scripts_dir(self, dir: PathBuf) -> Self` | Root that `_rift.script` `file:` references resolve under for imposters created through this API (issue #356). Without it such references are rejected. |
| `with_reported_admin_port` | `fn with_reported_admin_port(self, port: u16) -> Self` | Report `port` from `GET /config` instead of the port this server bound (issue #1135). For a host that fronts the admin API with its own public listener and binds the core to an ephemeral loopback port — without it, `options.port` advertises the *private* port to Mountebank-compat clients that read it to build URLs. Unset reports the bound port; `0` is a configured `0`, not "unset". `ServerBuilder::reported_admin_port` is the builder spelling. |
| `with_require_admin_auth` | `fn with_require_admin_auth(self, require: bool) -> Self` | Make `bind` **fail** when this server would be reachable off-host with no `api_key`, instead of warning (issue #863). The embedder spelling of `--require-admin-auth`. |
| `bind` | `async fn bind(self) -> anyhow::Result<RunningAdminApi>` | Bind and start serving; returns once bound. |
| `run` | `async fn run(self) -> anyhow::Result<()>` | Bind and serve until the accept loop exits. |

`bind` reports the authentication posture either way. When `addr` is **not** loopback and `api_key`
is `None`, it logs a warning naming the address and the remedies — the admin API can create
imposters and drive the TLS intercept proxy, so an unauthenticated off-host bind is worth stating
out loud. `with_require_admin_auth(true)` turns that warning into a startup error; the check runs
before the listener binds, so a refusal never leaves a socket behind. It gates on *authentication*,
not on the address: a real `api_key` satisfies it on any bind, and loopback satisfies it with none.

`RunningAdminApi`: `local_addr(&self) -> SocketAddr`, `shutdown(&self)`, `join(self) -> anyhow::Result<()>`,
`wait(&self) -> anyhow::Result<()>` (the non-consuming form of `join`, as above).

#### Computing `addr` the way the CLI does

If your binary parses rift's `Cli`, derive the admin address with `server::admin_bind_addr` rather
than reading `--host`/`--port` yourself — `--local-only` pins loopback and outranks `--host`, and a
private copy of that rule can disagree with the exposure check about *which address is being judged*
(issue #1131):

```rust
use rift_http_proxy::server::admin_bind_addr;

let addr = admin_bind_addr(&cli)?;          // --local-only pins 127.0.0.1, else --host, on --port
let running = AdminApiServer::new(addr, manager, cli.api_key.clone())
    .with_require_admin_auth(cli.require_admin_auth)
    .with_local_only(cli.local_only)
    .bind()
    .await?;
```

Do not call `check_admin_exposure` yourself here: `bind` already runs it on `addr`, so a hand-rolled
call in front of it judges the same address twice and logs the warning twice. Thread
`--require-admin-auth` through `with_require_admin_auth` instead — that is what turns the warning
into a refusal. (`ServerBuilder::start` *does* check before binding, but only because it has a
metrics listener it must not have to unwind; an embedder binding the admin plane alone does not.)

| Item | Signature | Purpose |
|:-----|:----------|:--------|
| `admin_bind_addr` | `fn admin_bind_addr(cli: &Cli) -> anyhow::Result<SocketAddr>` | The address the admin plane binds under this CLI: `--local-only` pins loopback, otherwise `--host`, on `--port`. The same value the exposure check is handed. `ServerBuilder::start` calls it too, so the rule has one definition. Accepts an IPv4 literal or an IPv6 literal, bare (`::1`) or bracketed (`[::1]`); errors when `--host` is not an IP literal (a DNS name is not resolved). |

`ConfigSource` (from `rift-http-proxy`) is either `File { path, no_parse }` (a single `--configfile`,
with optional EJS preprocessing) or `Dir(PathBuf)` (a `--datadir` of one-imposter-per-file configs).

### Metrics server

```rust
use rift_http_proxy::server::bind_metrics_server;

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

## Testing against a dead admin plane (`test-util`)

`RunningServer::wait()` reports the admin accept loop dying, which an embedder typically propagates
to process exit. Provoking that failure for real means breaking the listener, so the `test-util`
feature exposes constructors whose "accept loop" is a future you supply (issue #825):

```toml
[dev-dependencies]
rift-http-proxy = { version = "*", features = ["test-util"] }
```

```rust
use rift_http_proxy::server::RunningServer;

let server = RunningServer::with_admin_accept_task(async {
    Err(anyhow::anyhow!("admin plane died"))
});
let err = server.wait().await.expect_err("the embedder must see the failure");
```

`RunningAdminApi::with_accept_task` is the same seam one layer down. Neither binds a listener
(`admin_addr()` reports `127.0.0.1:0`), and both are test scaffolding — do not enable `test-util` in
a production build.

---

## Bootstrap helpers

`ServerBuilder` composes the *running* server, but a binary also has bootstrap concerns around it:
applying an rcfile's defaults, stopping a server by PID file, and saving a running server's
imposters. These live in `rift_http_proxy::bootstrap` so an alternative binary keeps CLI parity with
`rift` instead of reimplementing them (issue #807).

| Function | Signature | Purpose |
|:---------|:----------|:--------|
| `apply_rcfile_defaults` | `fn apply_rcfile_defaults(cli: &mut Cli, rcfile: &Path) -> anyhow::Result<()>` | Fill CLI fields **still at their clap defaults** from a Mountebank-compatible JSON rcfile. An explicitly-supplied flag always wins; unrecognised keys are logged with `warn!` and ignored. An `Err` means nothing was applied; the `rift` binary treats it as fatal. |
| `apply_rcfile_defaults_reporting` | `fn apply_rcfile_defaults_reporting(cli: &mut Cli, rcfile: &Path) -> anyhow::Result<Vec<String>>` | The same, returning the unrecognised keys instead of logging them — for a caller that applies the rcfile before it installs a log subscriber. |
| `stop_for_restart` | `fn stop_for_restart(pidfile: &Path) -> anyhow::Result<()>` | `stop_server`, except a missing PID file is a satisfied precondition (nothing to stop) rather than an error — the `restart` semantic. |
| `log_filter` | `fn log_filter(cli: &Cli) -> anyhow::Result<EnvFilter>` | The tracing filter this CLI asks for: `RUST_LOG` when set, otherwise `--debug`, otherwise `--loglevel`. Errors on a level that does not exist, and on a `RUST_LOG` that is set but does not parse — an unset `RUST_LOG` is an absence, not a failure (issue #1134). |
| `log_filter_with` | `fn log_filter_with(cli: &Cli, rust_log: Option<&str>) -> anyhow::Result<EnvFilter>` | The same rules with the `RUST_LOG` value supplied rather than read (`None` = unset), for a host that resolves it some other way — or wants to test without mutating the process environment. |
| `DEFAULT_PIDFILE` | `pub const DEFAULT_PIDFILE: &str` | The `rift.pid` fallback `stop`/`restart` apply when `--pidfile` is absent. Applied at the dispatch site so a plain start never writes a PID file it wasn't asked to. |
| `stop_server` | `fn stop_server(pidfile: &Path) -> anyhow::Result<()>` | Signal the process named in `pidfile` (SIGTERM on unix, `taskkill /F` on Windows), then remove the file. A stale pidfile (process already gone) is cleaned up as `Ok`; a denied or failed signal is an error and the pidfile is kept. On unix it waits up to five seconds for the process to exit; one still running after that is an error and the pidfile is kept. |
| `stop_server_within` | `fn stop_server_within(pidfile: &Path, ceiling: Duration) -> anyhow::Result<()>` | `stop_server` with the exit wait chosen by the caller — for an embedder whose server outlives five seconds on SIGTERM (a drain window, a cluster departure). |
| `save_imposters_async` | `async fn save_imposters_async(host: &str, port: u16, savefile: &Path, remove_proxies: bool, api_key: Option<&str>) -> anyhow::Result<()>` | Fetch `GET /imposters?replayable=true` from a running admin API and write it to `savefile`. The async form — call it from an embedder's own runtime. `api_key` is sent as the raw `Authorization` value; pass `None` for an unkeyed server (issue #1154). A non-2xx admin response is an error; nothing is written to `savefile`. |
| `save_imposters` | `fn save_imposters(host: &str, port: u16, savefile: &Path, remove_proxies: bool, api_key: Option<&str>) -> anyhow::Result<()>` | Blocking wrapper over `save_imposters_async` for the sync `save` subcommand path. |

Supported rcfile keys: `port`, `host`, `logLevel`/`loglevel`, `allowInjection`/`allow_injection`,
`localOnly`/`local_only`, `requireAdminAuth`/`require_admin_auth`, `apiKey`/`api_key`, `datadir`,
`configfile`, `noParse`/`no_parse`. Each must have its type — the flags are JSON booleans, `host`,
`logLevel`, `apiKey`, `datadir` and `configfile` are strings, and `port` is an integer from 0 to
65535 — or the whole rcfile is refused and nothing is applied.

`apiKey` sets the admin credential, like `--api-key`/`MB_APIKEY`, and like every other key it defers
to an explicitly-given flag (issue #1132). A blank value is refused by `validate_admin_api_key` at
startup exactly as a blank `--api-key` is — an rcfile is a normal place to keep the credential, so
keep it readable only by the user the server runs as.

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
        return bootstrap::save_imposters(
            &cli.host,
            cli.port,
            savefile,
            *remove_proxies,
            cli.api_key.as_deref(),
        );
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
