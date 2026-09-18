//! Rift HTTP Proxy - A Mountebank-compatible chaos engineering proxy
//!
//! Rift provides a Mountebank-compatible API with advanced features like:
//! - Probabilistic fault injection via `_rift.fault` extensions
//! - Multi-engine scripting (Rhai, JavaScript) via `_rift.script`
//! - Stateful testing with flow store via `_rift.flowState`
//!
//! # Examples
//!
//! Start Rift server:
//! ```bash
//! rift                                    # Admin API on port 2525
//! rift --port 3000                        # Admin API on port 3000
//! rift --configfile imposters.json        # Load imposters from file
//! rift --datadir ./mb-data                # Persist imposters to directory
//! ```
//!
//! The server composition itself (CLI surface, bootstrap, metrics, gateway dispatch) lives
//! in the `rift_http_proxy` library (issue #317); this binary is a thin caller.

// Route the server binary's allocations through mimalloc (issue #293). Gated by
// the default-on `mimalloc` feature so FFI/cross-compile builds can drop it; the
// allocator is set only here in the binary, never in the rift-mock-core/rift-ffi libs.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// jemalloc bake-off build (issue #717): active only when `jemalloc` is enabled and
// `mimalloc` is not — under `--all-features` (CI) mimalloc keeps precedence, so the
// two allocator features can coexist without a compile_error.
#[cfg(all(feature = "jemalloc", not(feature = "mimalloc")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Which global allocator this binary was built with — logged at startup so benchmark
/// results are labeled by the binary itself, not by whoever invoked the build (#717).
#[cfg(feature = "mimalloc")]
const ACTIVE_ALLOCATOR: &str = "mimalloc";
#[cfg(all(feature = "jemalloc", not(feature = "mimalloc")))]
const ACTIVE_ALLOCATOR: &str = "jemalloc";
#[cfg(not(any(feature = "mimalloc", feature = "jemalloc")))]
const ACTIVE_ALLOCATOR: &str = "system";

use anyhow::Context as _;
use clap::Parser;
use rift_http_proxy::bootstrap::{
    DEFAULT_PIDFILE, apply_rcfile_defaults_reporting, log_filter, save_imposters, stop_for_restart,
    stop_server,
};
use rift_http_proxy::healthcheck;
use rift_http_proxy::runtime;
use rift_http_proxy::script_cli;
use rift_http_proxy::server::{Cli, Commands, ServerBuilder};
use tracing::{info, warn};
use tracing_subscriber::{Layer, fmt, prelude::*};

fn main() -> Result<(), anyhow::Error> {
    let mut cli = Cli::parse();

    // Handle the `script` subcommand up front: no server bootstrap (tracing/rustls/rcfile),
    // just the CLI's own exit code (issue #360). Cloned rather than matched by value so `cli`
    // (and `cli.command`) stay intact for the Stop/Restart/Save/Replay dispatch below.
    if let Some(Commands::Script { action }) = cli.command.clone() {
        return script_cli::dispatch(action);
    }

    // Apply rcfile defaults before using CLI values (only for fields at their clap defaults).
    // A refused rcfile aborts startup (issue #1114): it was only warned about, so a mistyped
    // `requireAdminAuth` started the admin plane off-host with no auth and none of the file's keys.
    // `?` prints the whole error chain, so serde's line and column survive (#946/#1004). The log
    // subscriber is installed below, after the rcfile may have set the level, so warnings go to
    // stderr directly.
    //
    // Ahead of `healthcheck` (issue #1133), which computes its URL from `--host`/`--port`: a
    // deployment that sets the admin port in an rcfile otherwise ran a server on that port and a
    // probe that knocked on 2525 forever. This is not the "server bootstrap" the dispatch below
    // skips — that is the crypto provider and the tracing subscriber; reading one small JSON file
    // is cheap, and it is the one step whose *output* the probe depends on. A refused rcfile
    // therefore refuses the probe: a server started with that file would not start either, so
    // "unhealthy" is the true answer. `script` stays above, since it reads no host or port.
    if let Some(rcfile) = cli.rcfile.clone() {
        for key in apply_rcfile_defaults_reporting(&mut cli, &rcfile)? {
            eprintln!(
                "Warning: --rcfile {}: unsupported key '{key}' (ignored)",
                rcfile.display()
            );
        }
    }

    // The same treatment `script` gets above, for `healthcheck` (issue #664): skip the server
    // bootstrap entirely — but from below the rcfile, which it needs (issue #1133). (It used
    // to matter for a second reason — the path below wrote `--pidfile`, clobbering the running
    // server's PID file with the probe's own — but since #827 the PID file is written only on the
    // serving path, so a transient subcommand can no longer touch it.)
    if let Some(Commands::Healthcheck { url, timeout }) = cli.command.clone() {
        return healthcheck::dispatch(url, &cli.host, cli.port, timeout, cli.api_key.as_deref());
    }

    // `--debug` is the server-flag spelling of debug mode (issue #360 Item 3); `RIFT_DEBUG` is
    // the env-var spelling `rift_mock_core::util::rift_debug_env()` reads everywhere else (issue
    // #359). Setting it here (before anything calls `rift_debug_env()`, which caches its read)
    // makes both spellings equivalent. Safe: single-threaded, before the tokio runtime starts.
    if cli.debug {
        unsafe { std::env::set_var("RIFT_DEBUG", "1") };
    }

    // Install default cryptographic provider for rustls
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Failed to install default crypto provider"))?;

    // Initialize tracing based on loglevel. The rules live in the bootstrap seam (issue #1134) so
    // an alternative binary applies them rather than copying them: a level this binary does not
    // know is refused instead of becoming `info`, and a `RUST_LOG` that is set but unparseable is
    // refused instead of being mistaken for an unset one.
    let env_filter = log_filter(&cli)?;

    // Build optional file log layer when --log is set and --nologfile is not.
    //
    // The worker guard is held here, in `main`, and dropped when `main` returns — on every path,
    // `?` included — which is what flushes the non-blocking writer. It used to be `Box::leak`ed, so
    // it never dropped, and lines still queued when the process ended could be lost (issue #1155).
    let mut log_guard: Option<tracing_appender::non_blocking::WorkerGuard> = None;
    let file_layer: Option<Box<dyn Layer<_> + Send + Sync>> = if !cli.nologfile {
        cli.log.as_ref().and_then(|log_path| {
            let dir = log_path.parent().unwrap_or(std::path::Path::new("."));
            let filename = log_path.file_name()?.to_string_lossy().into_owned();
            let file_appender = tracing_appender::rolling::never(dir, filename);
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            log_guard = Some(guard);
            Some(fmt::layer().with_writer(non_blocking).boxed())
        })
    } else {
        None
    };
    // Named, not `_`: a `let _ =` binding would drop the guard immediately.
    let _log_guard = log_guard;

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .with(file_layer)
        .init();

    // Handle subcommands
    match &cli.command {
        Some(Commands::Stop) => {
            return stop_server(&pidfile_or_default(&cli));
        }
        Some(Commands::Restart) => {
            // A missing PID file is a satisfied precondition for restart, not an error (#827).
            stop_for_restart(&pidfile_or_default(&cli))?;
            // Fall through to start
        }
        Some(Commands::Save {
            savefile,
            remove_proxies,
        }) => {
            return save_imposters(
                &cli.host,
                cli.port,
                savefile,
                *remove_proxies,
                cli.api_key.as_deref(),
            );
        }
        Some(Commands::Replay { configfile }) => {
            // Load the config file and start
            return run_mountebank_mode(Cli {
                configfile: Some(configfile.clone()),
                ..cli
            });
        }
        // Already handled (and returned) above, before the server bootstrap; kept here so the
        // match stays exhaustive and correct if that ever changes.
        Some(Commands::Script { action }) => {
            return script_cli::dispatch(action.clone());
        }
        // Likewise already handled above — and it must stay that way: reaching here would mean the
        // probe had paid for the whole server bootstrap, and (since issue #1133) had computed its
        // target from `--host`/`--port` before `--rcfile` could set them.
        Some(Commands::Healthcheck { url, timeout }) => {
            return healthcheck::dispatch(
                url.clone(),
                &cli.host,
                cli.port,
                *timeout,
                cli.api_key.as_deref(),
            );
        }
        Some(Commands::Start) | None => {
            // Default behavior - start in Mountebank mode
        }
    }

    // Start in Mountebank mode
    info!("Starting Rift on port {}", cli.port);
    info!("Global allocator: {}", ACTIVE_ALLOCATOR);
    info!(
        "Matching dimensions: body-field(quamina)={}",
        if rift_mock_core::QUAMINA_BODY_FIELD_DIMENSION {
            "on"
        } else {
            "off"
        }
    );
    run_mountebank_mode(cli)
}

/// The PID file `stop`/`restart` act on: the single global `--pidfile` binding, falling back to
/// [`DEFAULT_PIDFILE`] (issue #827 — the default lives here, not on the flag).
fn pidfile_or_default(cli: &Cli) -> std::path::PathBuf {
    cli.pidfile
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_PIDFILE))
}

/// Run in Mountebank-compatible mode
fn run_mountebank_mode(cli: Cli) -> Result<(), anyhow::Error> {
    // Write the PID file here — the one place every serving entry converges (plain start, the
    // `restart` fall-through, and `Replay`'s re-entry). Writing it before the subcommand dispatch
    // meant `rift --pidfile p restart` recorded its OWN pid and then SIGTERMed itself, and a
    // transient `save`/`healthcheck` clobbered a running server's file (issue #827).
    if let Some(ref pidfile) = cli.pidfile {
        let pid = std::process::id();
        std::fs::write(pidfile, pid.to_string())?;
        info!("Wrote PID {} to {:?}", pid, pidfile);
    }
    // Remembered before `cli` moves into the builder: the server removes the PID file it wrote on
    // the way out, on success and on error alike (issue #1155). It never used to, so a Ctrl+C, a
    // plain `kill` or a `docker stop` all left a stale file behind.
    let written_pidfile = cli.pidfile.clone();
    let result = serve_topology(cli);
    if let Some(pidfile) = written_pidfile {
        remove_own_pidfile(&pidfile);
    }
    info!("stopped");
    result
}

/// Remove the PID file this process wrote — but only while it still names this process. A second
/// server started on the same `--pidfile` has overwritten it with its own PID by now, and deleting
/// that would orphan the live server from `rift stop`. Already gone is success: `rift stop` may have
/// got there first. Any other failure is logged, not turned into a changed exit code: the server
/// did stop.
fn remove_own_pidfile(pidfile: &std::path::Path) {
    let ours = std::process::id().to_string();
    match std::fs::read_to_string(pidfile) {
        Ok(contents) if contents.trim() == ours => {}
        Ok(_) => return,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::error!(error = %e, ?pidfile, "could not read the PID file to remove it");
            return;
        }
    }
    match std::fs::remove_file(pidfile) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::error!(error = %e, ?pidfile, "could not remove the PID file"),
    }
}

/// Pick the runtime topology and serve on it until the admin plane exits or a termination signal
/// arrives.
fn serve_topology(cli: Cli) -> Result<(), anyhow::Error> {
    // Topology selection (RFC-712, issue #744). Clap already applied RIFT_RUNTIME env fallback
    // into `cli.runtime`, so resolve() only sees the merged value; the platform gate then
    // downgrades or rejects per RFC D5 (macOS falls back with a warning, Windows refuses).
    let requested = runtime::RuntimeTopology::resolve(cli.runtime.as_deref(), None)
        .map_err(anyhow::Error::msg)?;
    let (topology, platform_warning) =
        runtime::platform_gate(requested, runtime::current_os()).map_err(anyhow::Error::msg)?;
    if let Some(warning) = platform_warning {
        warn!("{warning}");
    }
    info!("Runtime topology: {}", topology.describe());

    match topology {
        runtime::RuntimeTopology::WorkStealing => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(serve(ServerBuilder::from_cli(cli)));
            // Bounded, not an implicit drop — a stuck `decorate` would otherwise hold the process
            // after a graceful shutdown for ever. See `runtime::BLOCKING_DRAIN`.
            runtime.shutdown_timeout(runtime::BLOCKING_DRAIN);
            result
        }
        runtime::RuntimeTopology::PerCore { workers } => {
            // Control plane: admin API, metrics, savefile machinery, and imposter mutations
            // stay on one small multi-thread runtime; imposter accept loops fan out across
            // the workers (issue #745), verified live via the Ping handshake below.
            let control = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            let workers = runtime::WorkerSet::spawn(workers, cli.runtime_affinity)?;
            let total = workers.worker_count();
            let alive = control.block_on(workers.ping_all());
            if alive.len() != total {
                workers.shutdown();
                return Err(anyhow::anyhow!(
                    "per-core bootstrap: only {}/{total} workers came up; refusing to start degraded",
                    alive.len()
                ));
            }
            info!("Per-core workers up: {}", alive.len());
            // Imposter accept loops fan out across the workers (issue #745): the builder
            // threads the runtime handles into the manager, which binds one SO_REUSEPORT
            // listener per worker per imposter port.
            let result = control.block_on(serve(
                ServerBuilder::from_cli(cli).accept_runtimes(workers.handles()),
            ));
            workers.shutdown();
            control.shutdown_timeout(runtime::BLOCKING_DRAIN);
            result
        }
    }
}

/// Serve until the admin plane exits or a termination signal arrives (issue #1155).
///
/// The binary no longer calls `ServerBuilder::run`, which consumed the server and so left nothing
/// able to shut it down. Embedders own their process's signals, so `run` itself is unchanged; this
/// arm is the binary's.
///
/// On a signal the shutdown is `RunningServer::shutdown`: stop accepting, give in-flight admin,
/// metrics and front-door connections a bounded grace (about three seconds at worst), and exit 0 —
/// Mountebank's behaviour. It is deliberately **not** the FFI's `rift_stop`, which also calls
/// `ImposterManager::shutdown` and so deletes every imposter *and unlinks its `--datadir` file*:
/// wired to SIGTERM, that would make every `docker stop` wipe the datadir.
async fn serve(builder: ServerBuilder) -> anyhow::Result<()> {
    // Installed before the server starts, so a signal that lands during startup is held rather
    // than dropped — as PID 1, a SIGTERM with no handler is discarded by the kernel.
    let mut signals =
        TerminationSignals::install().context("installing the termination-signal handler")?;
    let server = builder.start().await?;
    tokio::select! {
        // The admin plane died on its own: surface it, and exit non-zero as before.
        result = server.wait() => return result,
        name = signals.recv() => info!(signal = name, "shutting down"),
    }
    server.shutdown().await;
    Ok(())
}

/// The termination signals the binary handles: SIGTERM and SIGINT on unix, Ctrl+C elsewhere.
struct TerminationSignals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl TerminationSignals {
    /// Register the handlers now. A failed install is an error, never ignored: a server that
    /// silently cannot be stopped gracefully is the defect being fixed.
    fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                term: signal(SignalKind::terminate())?,
                int: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    /// Resolve on the first signal, naming it for the log.
    async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => "SIGTERM",
                _ = self.int.recv() => "SIGINT",
            }
        }
        #[cfg(windows)]
        {
            self.ctrl_c.recv().await;
            "Ctrl+C"
        }
    }
}
