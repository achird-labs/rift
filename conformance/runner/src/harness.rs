//! The engine under test (port of `RiftHarness.scala` / `RiftHarnessEmbedded.scala`).
//!
//! One [`Harness`] per run owns the shared engine every scenario drives over HTTP. Its admin and
//! metrics URLs are fixed for the run. Scenarios that need an engine started with different flags
//! (feature 09 `@upstream-trust`) spawn a [`SpawnedEngine`] of their own beside it.

use crate::HarnessError;
use crate::build_info::BuildInfo;
use crate::tools::Tools;
use std::ffi::{CStr, CString, c_char};
use std::fmt;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

/// How long an engine has to answer its readiness probes (the Scala runner's budget).
const READINESS_BUDGET: Duration = Duration::from_secs(15);
const READINESS_SPACING: Duration = Duration::from_millis(100);
/// Spawn attempts for the shared engine. Each one uses fresh OS-assigned ports, so a port taken
/// between our probe and the engine's bind costs one retry, not the run.
const SPAWN_ATTEMPTS: usize = 5;

/// How the engine is delivered. `RIFT_MODE`, default `external`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Spawn the staged `rift` binary from `RIFT_BUILD_DIR`.
    External,
    /// Link `rift-ffi` and serve the real admin API in this process (`rift_serve_admin`).
    Embedded,
    /// Run the published image. Not implemented by this runner yet.
    Container,
}

impl Mode {
    pub fn parse(value: &str) -> Result<Self, HarnessError> {
        match value {
            "external" => Ok(Self::External),
            "embedded" => Ok(Self::Embedded),
            "container" => Ok(Self::Container),
            other => Err(HarnessError::InvalidEnv {
                name: "RIFT_MODE",
                value: other.to_string(),
                why: "expected external, embedded or container".to_string(),
            }),
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Embedded => "embedded",
            Self::Container => "container",
        }
    }

    /// Why this mode cannot run a scenario carrying `tag`, or `None` when it can. The tag
    /// vocabulary is the Scala runner's (`build.sbt`'s per-mode exclusions), so a feature file
    /// means the same thing to both runners.
    #[must_use]
    pub fn excludes(self, tag: &str) -> Option<&'static str> {
        match (self, tag) {
            (Self::Embedded, "upstream-trust") => Some(
                "starts a second engine with --upstream-ca-file, and only external mode spawns engines",
            ),
            _ => None,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The run's configuration, from the environment.
#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    /// `RIFT_BUILD_DIR`: `rift`, `rift-lint`, `rift-verify` and `BUILD_INFO`.
    pub build_dir: PathBuf,
    /// `RIFT_CORPUS_ROOT`: the JSON corpus, and the spawned engine's working directory.
    pub corpus_root: PathBuf,
    /// `RIFT_MAX_BODY_SIZE`, forwarded to the embedded engine (a spawned one reads the variable
    /// itself).
    pub max_body_size: Option<usize>,
    /// Where engine logs go for this run.
    pub log_dir: PathBuf,
}

impl Config {
    /// `None` when `RIFT_BUILD_DIR` is unset: the caller decides whether that is a skip or a
    /// failure.
    pub fn from_env(log_dir: PathBuf) -> Result<Option<Self>, HarnessError> {
        let Some(build_dir) = env_nonempty("RIFT_BUILD_DIR") else {
            return Ok(None);
        };
        let mode = match env_nonempty("RIFT_MODE") {
            Some(m) => Mode::parse(&m)?,
            None => Mode::External,
        };
        let corpus_root = env_nonempty("RIFT_CORPUS_ROOT")
            .map_or_else(crate::spec::default_corpus_root, PathBuf::from);
        let max_body_size = match env_nonempty("RIFT_MAX_BODY_SIZE") {
            None => None,
            Some(v) => Some(v.parse::<usize>().map_err(|e| HarnessError::InvalidEnv {
                name: "RIFT_MAX_BODY_SIZE",
                value: v.clone(),
                why: e.to_string(),
            })?),
        };
        Ok(Some(Self {
            mode,
            build_dir: absolute(PathBuf::from(build_dir))?,
            corpus_root: absolute(corpus_root)?,
            max_body_size,
            log_dir,
        }))
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn absolute(path: PathBuf) -> Result<PathBuf, HarnessError> {
    std::fs::canonicalize(&path).map_err(|source| HarnessError::Io {
        action: "resolving",
        path,
        source,
    })
}

/// A port nothing is listening on right now, chosen by the OS. There is a window between this
/// probe releasing it and the engine binding it; callers that hand it to an engine retry on a
/// failed start rather than trusting it.
pub fn free_port() -> Result<u16, HarnessError> {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(HarnessError::NoFreePort)
}

/// The run's shared engine.
pub struct Harness {
    pub config: Config,
    pub admin_url: String,
    pub metrics_url: String,
    pub tools: Tools,
    pub build_info: BuildInfo,
    engine: tokio::sync::Mutex<Option<Engine>>,
}

enum Engine {
    External(SpawnedEngine),
    Embedded(EmbeddedEngine),
}

impl Harness {
    /// Start the shared engine for `config.mode`. Container mode is refused here: the caller
    /// reports it as not implemented before getting this far.
    pub async fn start(config: Config) -> Result<Self, HarnessError> {
        let build_info = BuildInfo::load(&config.build_dir)?;
        let tools = Tools::from_build_dir(&config.build_dir);
        let engine = match config.mode {
            Mode::External => Engine::External(spawn_with_retry(&config, &[]).await?),
            Mode::Embedded => Engine::Embedded(EmbeddedEngine::start(&config).await?),
            Mode::Container => {
                return Err(HarnessError::Unsupported {
                    what: "the shared engine",
                    mode: "container",
                });
            }
        };
        let (admin_url, metrics_url) = match &engine {
            Engine::External(e) => (e.admin_url.clone(), e.metrics_url.clone()),
            Engine::Embedded(e) => (e.admin_url.clone(), e.metrics_url.clone()),
        };
        Ok(Self {
            config,
            admin_url,
            metrics_url,
            tools,
            build_info,
            engine: tokio::sync::Mutex::new(Some(engine)),
        })
    }

    /// The host imposters are reached on. Imposters bind real host ports in both modes.
    #[must_use]
    pub fn imposter_host(&self) -> &'static str {
        "localhost"
    }

    /// Make a fixture's engine-cwd-relative data paths (a `lookup`'s `"data/products.csv"`)
    /// resolve. A spawned engine runs in the corpus root, so they already do; the embedded engine
    /// shares this process's working directory, so they are made absolute under the corpus root.
    /// Predicate paths start with `/`, so a leading `"data/` is only ever a data-source value.
    #[must_use]
    pub fn rewrite_fixture(&self, json: &str) -> String {
        match self.config.mode {
            Mode::Embedded => json.replace(
                "\"data/",
                &format!("\"{}/data/", self.config.corpus_root.display()),
            ),
            Mode::External | Mode::Container => json.to_string(),
        }
    }

    /// Start a dedicated engine with `--upstream-ca-file <ca>` (feature 09 `@upstream-trust`,
    /// rift#974). The caller owns it; dropping it kills the process.
    pub async fn spawn_upstream_trust_engine(
        &self,
        ca_file: &Path,
    ) -> Result<SpawnedEngine, HarnessError> {
        if self.config.mode != Mode::External {
            return Err(HarnessError::Unsupported {
                what: "the upstream-trust engine",
                mode: self.config.mode.name(),
            });
        }
        let args = vec![
            "--upstream-ca-file".to_string(),
            ca_file.display().to_string(),
        ];
        spawn_with_retry(&self.config, &args).await
    }

    /// Kill the shared engine synchronously, without waiting for it: the teardown of last resort,
    /// for a drop guard on a path (a panic) where [`Harness::shutdown`] cannot be awaited. Does
    /// nothing if the engine is already stopped or a shutdown holds it.
    pub fn kill_now(&self) {
        let Ok(mut slot) = self.engine.try_lock() else {
            return;
        };
        match slot.take() {
            Some(Engine::External(mut e)) => {
                if let Err(err) = e.child.start_kill() {
                    eprintln!(
                        "plane_a: killing the engine (log {}): {err}",
                        e.log.display()
                    );
                }
            }
            // rift_stop drives its own runtime with block_on, which may not run on a thread that is
            // inside ours, so it runs on a plain thread of its own.
            Some(Engine::Embedded(e)) => {
                let handle = e.handle;
                let stopped = std::thread::spawn(move || {
                    // SAFETY: the handle came from rift_start and this is its only rift_stop.
                    unsafe { rift_ffi::rift_stop(handle.into_raw()) };
                })
                .join();
                if stopped.is_err() {
                    eprintln!("plane_a: rift_stop panicked while tearing the embedded engine down");
                }
            }
            None => {}
        }
    }

    /// Stop the shared engine. Safe to call twice; the second call does nothing.
    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        let engine = self.engine.lock().await.take();
        match engine {
            Some(Engine::External(e)) => e.stop().await,
            Some(Engine::Embedded(e)) => e.stop().await,
            None => Ok(()),
        }
    }
}

/// A `rift` child process, killed on drop. That covers the engines a scenario owns; the shared
/// engine lives in the [`Harness`] for the whole run, which is torn down by [`Harness::shutdown`]
/// or, on a panic, [`Harness::kill_now`].
#[derive(Debug)]
pub struct SpawnedEngine {
    child: Child,
    pub admin_url: String,
    pub metrics_url: String,
    pub log: PathBuf,
}

async fn spawn_with_retry(
    config: &Config,
    extra: &[String],
) -> Result<SpawnedEngine, HarnessError> {
    let mut failures = Vec::new();
    for _ in 0..SPAWN_ATTEMPTS {
        match SpawnedEngine::start(config, extra).await {
            Ok(engine) => return Ok(engine),
            Err(e) => failures.push(e.to_string()),
        }
    }
    Err(HarnessError::NotReady(format!(
        "{SPAWN_ATTEMPTS} attempts failed:\n{}",
        failures.join("\n")
    )))
}

impl SpawnedEngine {
    /// Spawn `rift --port <admin> --nologfile --metrics-port <m> --allow-injection <extra>` in the
    /// corpus root, output to a log file, and wait until its admin and metrics listeners answer.
    async fn start(config: &Config, extra: &[String]) -> Result<Self, HarnessError> {
        let admin_port = free_port()?;
        let metrics_port = free_port()?;
        let log = config.log_dir.join(format!("engine-{admin_port}.log"));
        let log_file = std::fs::File::create(&log).map_err(|source| HarnessError::Io {
            action: "creating",
            path: log.clone(),
            source,
        })?;
        let log_err = log_file.try_clone().map_err(|source| HarnessError::Io {
            action: "duplicating",
            path: log.clone(),
            source,
        })?;
        let binary = config.build_dir.join("rift");
        let child = Command::new(&binary)
            .arg("--port")
            .arg(admin_port.to_string())
            .arg("--nologfile")
            .arg("--metrics-port")
            .arg(metrics_port.to_string())
            .arg("--allow-injection")
            .args(extra)
            .current_dir(&config.corpus_root)
            .stdin(Stdio::null())
            .stdout(log_file)
            .stderr(log_err)
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| HarnessError::Io {
                action: "spawning",
                path: binary,
                source,
            })?;
        let mut engine = Self {
            child,
            admin_url: format!("http://localhost:{admin_port}"),
            metrics_url: format!("http://localhost:{metrics_port}"),
            log,
        };
        match engine.await_ready().await {
            Ok(()) => Ok(engine),
            Err(e) => {
                // A never-ready engine must not keep its ports for the rest of the run.
                match engine.stop().await {
                    Ok(()) => Err(e),
                    Err(stop) => Err(HarnessError::NotReady(format!(
                        "{e}\nand stopping that engine then failed too: {stop}"
                    ))),
                }
            }
        }
    }

    async fn await_ready(&mut self) -> Result<(), HarnessError> {
        let probe = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(HarnessError::Client)?;
        let urls = [
            format!("{}/imposters", self.admin_url),
            format!("{}/metrics", self.metrics_url),
        ];
        let deadline = Instant::now() + READINESS_BUDGET;
        for url in &urls {
            loop {
                if let Some(status) = self.child.try_wait().map_err(|source| HarnessError::Io {
                    action: "waiting on",
                    path: self.log.clone(),
                    source,
                })? {
                    return Err(HarnessError::NotReady(format!(
                        "rift exited ({status}) before {url} answered; log {}:\n{}",
                        self.log.display(),
                        log_tail(&self.log)
                    )));
                }
                let last = match probe.get(url).send().await {
                    Ok(r) if r.status().is_success() => break,
                    Ok(r) => format!("HTTP {}", r.status()),
                    Err(e) => e.to_string(),
                };
                if Instant::now() >= deadline {
                    return Err(HarnessError::NotReady(format!(
                        "{url} did not answer 200 within {READINESS_BUDGET:?} (last: {last}); log {}:\n{}",
                        self.log.display(),
                        log_tail(&self.log)
                    )));
                }
                tokio::time::sleep(READINESS_SPACING).await;
            }
        }
        Ok(())
    }

    /// Kill the process and reap it. Fails loudly if it survives: "no orphan engine" is the
    /// harness's whole job.
    pub async fn stop(mut self) -> Result<(), HarnessError> {
        let pid = self.child.id();
        // An already-exited child makes start_kill fail with InvalidInput; that is the goal state.
        if let Err(e) = self.child.start_kill()
            && e.kind() != std::io::ErrorKind::InvalidInput
        {
            return Err(HarnessError::Io {
                action: "killing the engine logging to",
                path: self.log.clone(),
                source: e,
            });
        }
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(source)) => Err(HarnessError::Io {
                action: "reaping the engine logging to",
                path: self.log.clone(),
                source,
            }),
            Err(_) => Err(HarnessError::SurvivedTeardown {
                pid: pid.unwrap_or_default(),
            }),
        }
    }
}

/// The last 40 lines of an engine log, for a failure message.
#[must_use]
pub fn log_tail(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(40)..].join("\n")
        }
        Err(e) => format!("(could not read the log: {e})"),
    }
}

/// The engine running in this process through rift-ffi's C-ABI, exactly as an SDK embeds it.
struct EmbeddedEngine {
    handle: HandlePtr,
    admin_url: String,
    metrics_url: String,
}

/// `rift_start`'s opaque handle. Every C-ABI entry point takes it from any thread (the SDKs call
/// them from arbitrary JVM/Go/Node threads); it is only ever passed back to them.
struct HandlePtr(*mut rift_ffi::RiftHandle);
// SAFETY: the handle is an opaque pointer into rift-ffi, whose entry points are documented as
// callable from any thread; this runner never dereferences it.
unsafe impl Send for HandlePtr {}

impl HandlePtr {
    /// Moving the whole wrapper into a closure (not its field) is what keeps it `Send`.
    fn into_raw(self) -> *mut rift_ffi::RiftHandle {
        self.0
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServeAdminReply {
    admin_url: String,
    metrics_port: Option<u16>,
}

impl EmbeddedEngine {
    async fn start(config: &Config) -> Result<Self, HarnessError> {
        let mut options = serde_json::json!({
            "host": "127.0.0.1",
            "port": 0,
            "metricsPort": 0,
            "allowInjection": true,
        });
        if let Some(max) = config.max_body_size {
            options["maxBodySize"] = max.into();
        }
        let options = CString::new(options.to_string())
            .map_err(|e| HarnessError::Embedded(format!("options JSON: {e}")))?;
        // rift-ffi drives its own tokio runtime with `block_on`, which may not run on a thread of
        // ours that is already inside a runtime: blocking pool threads are not.
        let (handle, reply) = tokio::task::spawn_blocking(move || {
            let handle = rift_ffi::rift_start();
            if handle.is_null() {
                return Err(HarnessError::Embedded(format!(
                    "rift_start: {}",
                    last_error()
                )));
            }
            // SAFETY: `handle` is the live handle rift_start just returned and `options` a valid
            // NUL-terminated string that outlives the call.
            let reply = unsafe { rift_ffi::rift_serve_admin(handle, options.as_ptr()) };
            if reply.is_null() {
                let err = last_error();
                // SAFETY: the handle is live and is not used again.
                unsafe { rift_ffi::rift_stop(handle) };
                return Err(HarnessError::Embedded(format!("rift_serve_admin: {err}")));
            }
            // SAFETY: a non-null reply is a NUL-terminated string rift-ffi allocated; it is copied
            // here and then freed exactly once with rift_free.
            let text = unsafe { CStr::from_ptr(reply) }
                .to_string_lossy()
                .into_owned();
            unsafe { rift_ffi::rift_free(reply) };
            Ok((HandlePtr(handle), text))
        })
        .await
        .map_err(|e| HarnessError::Embedded(format!("start task: {e}")))??;
        let reply: ServeAdminReply = match serde_json::from_str(&reply) {
            Ok(r) => r,
            Err(e) => {
                let why = format!("rift_serve_admin reply {reply:?}: {e}");
                return Err(with_stop_outcome(why, stop_handle(handle).await));
            }
        };
        let Some(metrics_port) = reply.metrics_port else {
            let why = "rift_serve_admin started no metrics listener for metricsPort 0".to_string();
            return Err(with_stop_outcome(why, stop_handle(handle).await));
        };
        Ok(Self {
            handle,
            admin_url: reply.admin_url,
            metrics_url: format!("http://127.0.0.1:{metrics_port}"),
        })
    }

    async fn stop(self) -> Result<(), HarnessError> {
        stop_handle(self.handle).await
    }
}

/// A start failure, keeping the stop failure too when tearing the half-started engine down failed.
fn with_stop_outcome(why: String, stopped: Result<(), HarnessError>) -> HarnessError {
    match stopped {
        Ok(()) => HarnessError::Embedded(why),
        Err(stop) => {
            HarnessError::Embedded(format!("{why}\nand rift_stop then failed too: {stop}"))
        }
    }
}

async fn stop_handle(handle: HandlePtr) -> Result<(), HarnessError> {
    tokio::task::spawn_blocking(move || {
        // SAFETY: the handle came from rift_start and this is its only rift_stop.
        unsafe { rift_ffi::rift_stop(handle.into_raw()) };
    })
    .await
    .map_err(|e| HarnessError::Embedded(format!("rift_stop task: {e}")))
}

/// rift-ffi's thread-local last error, or a note that there was none.
fn last_error() -> String {
    let p: *mut c_char = rift_ffi::rift_last_error();
    if p.is_null() {
        return "(no rift_last_error)".to_string();
    }
    // SAFETY: a non-null rift_last_error is a NUL-terminated string the caller frees once.
    let text = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { rift_ffi::rift_free(p) };
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_the_three_names_and_refuses_others() {
        assert_eq!(Mode::parse("external").ok(), Some(Mode::External));
        assert_eq!(Mode::parse("embedded").ok(), Some(Mode::Embedded));
        assert_eq!(Mode::parse("container").ok(), Some(Mode::Container));
        assert!(Mode::parse("ffm").is_err());
    }

    #[test]
    fn only_embedded_excludes_upstream_trust() {
        assert!(Mode::Embedded.excludes("upstream-trust").is_some());
        assert!(Mode::External.excludes("upstream-trust").is_none());
        assert!(Mode::Embedded.excludes("full-corpus-verify").is_none());
    }

    #[test]
    fn free_port_is_not_zero() {
        assert_ne!(free_port().expect("a free port"), 0);
    }
}
