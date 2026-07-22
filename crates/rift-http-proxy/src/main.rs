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

use clap::Parser;
use rift_http_proxy::bootstrap::{
    DEFAULT_PIDFILE, apply_rcfile_defaults, save_imposters, stop_for_restart, stop_server,
};
use rift_http_proxy::healthcheck;
use rift_http_proxy::runtime;
use rift_http_proxy::script_cli;
use rift_http_proxy::server::{Cli, Commands, ServerBuilder};
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, Layer, fmt, prelude::*};

fn main() -> Result<(), anyhow::Error> {
    let mut cli = Cli::parse();

    // Handle the `script` subcommand up front: no server bootstrap (tracing/rustls/rcfile),
    // just the CLI's own exit code (issue #360). Cloned rather than matched by value so `cli`
    // (and `cli.command`) stay intact for the Stop/Restart/Save/Replay dispatch below.
    if let Some(Commands::Script { action }) = cli.command.clone() {
        return script_cli::dispatch(action);
    }

    // Same treatment for `healthcheck` (issue #664): skip the server bootstrap entirely. (It used
    // to matter for a second reason — the path below wrote `--pidfile`, clobbering the running
    // server's PID file with the probe's own — but since #827 the PID file is written only on the
    // serving path, so a transient subcommand can no longer touch it.)
    if let Some(Commands::Healthcheck { url, timeout }) = cli.command.clone() {
        return healthcheck::dispatch(url, &cli.host, cli.port, timeout);
    }

    // `--debug` is the server-flag spelling of debug mode (issue #360 Item 3); `RIFT_DEBUG` is
    // the env-var spelling `rift_mock_core::util::rift_debug_env()` reads everywhere else (issue
    // #359). Setting it here (before anything calls `rift_debug_env()`, which caches its read)
    // makes both spellings equivalent. Safe: single-threaded, before the tokio runtime starts.
    if cli.debug {
        unsafe { std::env::set_var("RIFT_DEBUG", "1") };
    }

    // Apply rcfile defaults before using CLI values (only for fields at their clap defaults)
    if let Some(ref rcfile) = cli.rcfile.clone() {
        match apply_rcfile_defaults(&mut cli, rcfile) {
            Ok(()) => {}
            Err(e) => eprintln!("Warning: failed to load --rcfile {rcfile:?}: {e}"),
        }
    }

    // Install default cryptographic provider for rustls
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Failed to install default crypto provider"))?;

    // Initialize tracing based on loglevel
    let log_level = match cli.loglevel.to_lowercase().as_str() {
        "debug" => "debug",
        "warn" | "warning" => "warn",
        "error" => "error",
        _ => "info",
    };

    let filter = if cli.debug { "debug" } else { log_level };
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    // Build optional file log layer when --log is set and --nologfile is not
    let file_layer: Option<Box<dyn Layer<_> + Send + Sync>> = if !cli.nologfile {
        cli.log.as_ref().and_then(|log_path| {
            let dir = log_path.parent().unwrap_or(std::path::Path::new("."));
            let filename = log_path.file_name()?.to_string_lossy().into_owned();
            let file_appender = tracing_appender::rolling::never(dir, filename);
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            // Leak the guard so it lives for the process lifetime
            Box::leak(Box::new(guard));
            Some(fmt::layer().with_writer(non_blocking).boxed())
        })
    } else {
        None
    };

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
            return save_imposters(&cli.host, cli.port, savefile, *remove_proxies);
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
        // probe had already overwritten `--pidfile` with its own PID.
        Some(Commands::Healthcheck { url, timeout }) => {
            return healthcheck::dispatch(url.clone(), &cli.host, cli.port, *timeout);
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
            runtime.block_on(ServerBuilder::from_cli(cli).run())
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
            let result = control.block_on(
                ServerBuilder::from_cli(cli)
                    .accept_runtimes(workers.handles())
                    .run(),
            );
            workers.shutdown();
            result
        }
    }
}
