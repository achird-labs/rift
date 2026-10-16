//! The engine under test (port of `RiftHarness.scala` / `RiftHarnessEmbedded.scala` /
//! `RiftHarnessContainer.scala`).
//!
//! One [`Harness`] per run owns the shared engine every scenario drives over HTTP. Its admin and
//! metrics URLs are fixed for the run. Scenarios that need an engine started with different flags
//! (feature 09 `@upstream-trust`, features 05 and 19 `@rift-debug-env`) spawn a [`SpawnedEngine`]
//! of their own beside it, feature 11's reload scenarios start a [`ReloadEngine`] serving a config
//! file of their own, feature 21's `@admin-isolated` ones an empty [`IsolatedEngine`], and feature
//! 12's an [`IsolatedEngine`] with no intercept listener or an [`InterceptEngine`] with one of its
//! own. Container mode starts none of them: [`Mode::excludes`] leaves those scenarios out.
//!
//! The shared engine runs an intercept listener in every mode (feature 12), seeded with the
//! committed test CA (`rift-fixtures/intercept/`), so the CA it exports is that one.

use crate::HarnessError;
use crate::build_info::BuildInfo;
use crate::tools::Tools;
use std::ffi::{CStr, CString, c_char};
use std::fmt;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

/// How long an engine has to answer its readiness probes (the Scala runner's budget).
const READINESS_BUDGET: Duration = Duration::from_secs(15);
const READINESS_SPACING: Duration = Duration::from_millis(100);
/// Spawn attempts for a spawned engine, so one failed start costs a retry, not the run.
const SPAWN_ATTEMPTS: usize = 5;

/// Container mode's fixed ports inside the container. Admin and metrics are published on free host
/// ports (a developer machine often has Mountebank on 2525); the imposter ranges 1:1.
const CONTAINER_ADMIN_PORT: u16 = 2525;
const CONTAINER_METRICS_PORT: u16 = 4790;
/// The intercept listener inside the container, published on a free host port like admin.
const CONTAINER_INTERCEPT_PORT: u16 = 8443;
/// Where the test CA directory is mounted in the container.
const CONTAINER_CA_DIR: &str = "/intercept-ca";
/// Published 1:1 because fixtures and the corpus name ports in them and the engine reports the port
/// it bound: 4500-4599 the corpus (4501-4521), 4800-4899 the fixtures' fixed ports (4800-4829,
/// 4870-4879) and the pool.
const CONTAINER_IMPOSTER_RANGES: [(u16, u16); 2] = [(4500, 4599), (4800, 4899)];
/// The fresh-imposter pool: the top of the published ranges, clear of every fixture's fixed port.
const CONTAINER_POOL: (u16, u16) = (4880, 4899);
/// The image's WORKDIR, where the corpus's `data/` is mounted so `data/...` paths resolve as they do
/// against the spawned engine's cwd.
const IMAGE_WORKDIR: &str = "/data";
/// Every container this runner starts carries it, so one leaked by an interrupted run is findable.
const CONTAINER_LABEL: &str = "rift-conformance=1";
/// Where every engine this runner starts is addressed (see [`Harness::imposter_host`]).
const LOOPBACK: &str = "127.0.0.1";

/// How the engine is delivered. `RIFT_MODE`, default `external`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Spawn the staged `rift` binary from `RIFT_BUILD_DIR`.
    External,
    /// Link `rift-ffi` and serve the real admin API in this process (`rift_serve_admin`).
    Embedded,
    /// Run the engine image `RIFT_IMAGE` with Docker, imposter ports published 1:1.
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
    /// vocabulary is the one the Scala runner's `build.sbt` excluded per mode, kept so the feature
    /// files did not change in the move.
    #[must_use]
    pub fn excludes(self, tag: &str) -> Option<&'static str> {
        match (self, tag) {
            (Self::Embedded, "upstream-trust") => Some(
                "starts a second engine with --upstream-ca-file, which this harness only spawns in external mode",
            ),
            (Self::Embedded, "front-door") => Some(
                "drives the front door of `rift --front-door`, which rift_serve_admin has no option for",
            ),
            (Self::Container, "upstream-trust") => Some(
                "starts a second engine with --upstream-ca-file, which this harness only spawns in external mode",
            ),
            (Self::Container, "front-door") => Some(
                "starts its own `rift --front-door` engine, which this harness only spawns in external mode",
            ),
            (Self::Container, "full-corpus-verify") => Some(
                "rift-verify --verify-dynamic has the engine bind imposters on dynamic ports, which the container does not publish",
            ),
            (Self::Container, "loopback-client") => Some(
                "Docker's port forwarding rewrites the client address, so the engine never sees a loopback peer",
            ),
            (Self::Container, "connect-refused") => Some(
                "a published port always accepts on the host, so a connection is never refused",
            ),
            (Self::Container, "host-port") => Some(
                "an engine-assigned port lands outside the published ranges, unreachable from the host",
            ),
            (Self::Container, "tcp-rst") => {
                Some("Docker's port forwarder turns the engine's RST into an orderly EOF")
            }
            (Self::Container, "reload-engine") => Some(
                "starts an engine of its own serving a config file, which this harness doesn't start in a container",
            ),
            (Self::Embedded, "rift-debug-env") => Some(
                "needs debug mode, which is RIFT_DEBUG read once per process, so an in-process engine cannot have it for one scenario",
            ),
            (Self::Container, "rift-debug-env") => Some(
                "starts an engine of its own with --debug, which this harness doesn't start in a container",
            ),
            (Self::Container, "admin-isolated") => Some(
                "starts a second engine of its own, which this harness doesn't start in a container",
            ),
            (Self::Container, "intercept-runtime") => Some(
                "starts an engine of its own with no intercept listener, which this harness doesn't start in a container",
            ),
            (Self::Container, "intercept-auth") => Some(
                "starts an engine of its own whose intercept listener requires proxy auth, which this harness doesn't start in a container",
            ),
            (Self::Container, "intercept-ws") => Some(
                "starts an intercept engine of its own that dials an origin on the host's loopback, which a container cannot reach",
            ),
            (Self::Container, "intercept-upstream-trust") => Some(
                "starts an intercept engine of its own trusting the test CA upstream, which this harness doesn't start in a container",
            ),
            (Self::Embedded, "tls-flags") => Some(
                "starts an engine with a TLS CLI flag or environment variable, which an in-process engine cannot take per scenario",
            ),
            (Self::Container, "tls-flags") => Some(
                "starts an engine of its own with a TLS CLI flag or environment variable, which this harness doesn't start in a container",
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
    /// `RIFT_IMAGE`: the engine image container mode runs. Required there, unused elsewhere.
    pub image: Option<String>,
}

impl Config {
    /// `None` when `RIFT_BUILD_DIR` is unset: the caller decides whether that is a skip or a
    /// failure.
    pub fn from_env(log_dir: PathBuf) -> Result<Option<Self>, HarnessError> {
        Self::from_vars(env_nonempty, log_dir)
    }

    /// [`Config::from_env`] over `var`, which returns a variable's trimmed, non-empty value.
    pub fn from_vars(
        var: impl Fn(&str) -> Option<String>,
        log_dir: PathBuf,
    ) -> Result<Option<Self>, HarnessError> {
        let Some(build_dir) = var("RIFT_BUILD_DIR") else {
            return Ok(None);
        };
        let mode = match var("RIFT_MODE") {
            Some(m) => Mode::parse(&m)?,
            None => Mode::External,
        };
        let corpus_root =
            var("RIFT_CORPUS_ROOT").map_or_else(crate::spec::default_corpus_root, PathBuf::from);
        let max_body_size = match var("RIFT_MAX_BODY_SIZE") {
            None => None,
            Some(v) => Some(v.parse::<usize>().map_err(|e| HarnessError::InvalidEnv {
                name: "RIFT_MAX_BODY_SIZE",
                value: v.clone(),
                why: e.to_string(),
            })?),
        };
        let corpus_root = absolute(corpus_root)?;
        let image = match mode {
            Mode::Container => {
                let image = var("RIFT_IMAGE").ok_or_else(missing_image)?;
                // Docker would bind-mount a missing source as a new, empty, root-owned directory.
                let data = corpus_root.join("data");
                if !data.is_dir() {
                    return Err(HarnessError::InvalidEnv {
                        name: "RIFT_CORPUS_ROOT",
                        value: corpus_root.display().to_string(),
                        why: format!(
                            "container mode mounts {} into the image, and it is not a directory",
                            data.display()
                        ),
                    });
                }
                Some(image)
            }
            Mode::External | Mode::Embedded => None,
        };
        Ok(Some(Self {
            mode,
            build_dir: absolute(PathBuf::from(build_dir))?,
            corpus_root,
            max_body_size,
            log_dir,
            image,
        }))
    }
}

/// Never a default image: silently testing an older engine is exactly what container mode guards.
fn missing_image() -> HarnessError {
    HarnessError::MissingEnv {
        name: "RIFT_IMAGE",
        why: "RIFT_MODE=container runs that image; build it from this tree (see conformance/README.md)",
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

/// A port nothing is listening on right now, chosen by the OS. A spawned engine binds port 0 and
/// reports its own ports instead (#1485); this is left for ports no engine binds before it is
/// handed out: the host side of a container's `-p 127.0.0.1:<port>:…` publish (Docker binds that
/// loopback address, the family probed here), and a port a scenario needs nothing listening on.
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
    /// The shared engine's intercept listener, on [`Harness::imposter_host`].
    pub intercept_port: u16,
    pub tools: Tools,
    pub build_info: BuildInfo,
    engine: tokio::sync::Mutex<Option<Engine>>,
    port_pool: Option<PortPool>,
}

enum Engine {
    External(SpawnedEngine),
    Embedded(EmbeddedEngine),
    Container(ContainerEngine),
}

impl Harness {
    /// Start the shared engine for `config.mode`.
    pub async fn start(config: Config) -> Result<Self, HarnessError> {
        let build_info = BuildInfo::load(&config.build_dir)?;
        let tools = Tools::from_build_dir(&config.build_dir);
        let engine = match config.mode {
            Mode::External => {
                let opts = SpawnOptions {
                    intercept: true,
                    ..SpawnOptions::default()
                };
                Engine::External(spawn_with_retry(&config, &opts).await?)
            }
            Mode::Embedded => {
                let e = EmbeddedEngine::start(&config, &EmbeddedOptions::default()).await?;
                Engine::Embedded(e.with_test_ca_intercept(None).await?)
            }
            Mode::Container => {
                let image = config.image.as_deref().ok_or_else(missing_image)?;
                Engine::Container(ContainerEngine::start(&config, image).await?)
            }
        };
        let (admin_url, metrics_url, intercept_port) = match &engine {
            Engine::External(e) => (e.admin_url.clone(), e.metrics_url.clone(), e.intercept_port),
            Engine::Embedded(e) => (e.admin_url.clone(), e.metrics_url.clone(), e.intercept_port),
            Engine::Container(e) => (
                e.admin_url.clone(),
                e.metrics_url.clone(),
                Some(e.intercept_port),
            ),
        };
        let Some(intercept_port) = intercept_port else {
            return Err(HarnessError::NotReady(
                "the shared engine started without its intercept listener".to_string(),
            ));
        };
        let port_pool = match config.mode {
            Mode::Container => Some(PortPool::new(CONTAINER_POOL.0, CONTAINER_POOL.1)),
            Mode::External | Mode::Embedded => None,
        };
        Ok(Self {
            config,
            admin_url,
            metrics_url,
            intercept_port,
            tools,
            build_info,
            engine: tokio::sync::Mutex::new(Some(engine)),
            port_pool,
        })
    }

    /// Where a fresh imposter's port comes from: `None` lets the engine assign one (external,
    /// embedded); in container mode an engine-assigned port would land outside the published ranges,
    /// so the port is taken from this pool instead.
    #[must_use]
    pub fn imposter_port_pool(&self) -> Option<&PortPool> {
        self.port_pool.as_ref()
    }

    /// The host imposters are reached on: the loopback the engine binds in external and embedded
    /// mode, and the one the container's ports are published 1:1 on in container mode. IPv4 by
    /// address, never `localhost`: macOS resolves that to `::1` first, where an unrelated process
    /// may hold the same port while the engine listens on IPv4 only.
    #[must_use]
    pub fn imposter_host(&self) -> &'static str {
        LOOPBACK
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
        let opts = SpawnOptions {
            args: vec![
                "--upstream-ca-file".to_string(),
                ca_file.display().to_string(),
            ],
            ..SpawnOptions::default()
        };
        spawn_with_retry(&self.config, &opts).await
    }

    /// Start a dedicated engine with the extra CLI `flags` (features 05 and 19 `@rift-debug-env`:
    /// `--debug`). The caller owns it; dropping it kills the process.
    pub async fn spawn_flag_engine(&self, flags: &[String]) -> Result<SpawnedEngine, HarnessError> {
        self.spawn_flag_engine_with_env(flags, &[]).await
    }

    /// [`Harness::spawn_flag_engine`] with `env` set in the engine's environment too (feature 20
    /// `@tls-flags`: `RIFT_DISABLE_HTTP2`).
    pub async fn spawn_flag_engine_with_env(
        &self,
        flags: &[String],
        env: &[(String, String)],
    ) -> Result<SpawnedEngine, HarnessError> {
        if self.config.mode != Mode::External {
            return Err(HarnessError::Unsupported {
                what: "an engine started with extra flags",
                mode: self.config.mode.name(),
            });
        }
        let opts = SpawnOptions {
            args: flags.to_vec(),
            env: env.to_vec(),
            ..SpawnOptions::default()
        };
        spawn_with_retry(&self.config, &opts).await
    }

    /// Boot an engine with `flags` (and `--front-door 0` when `front_door`) once, and wait up to
    /// the readiness budget for it to exit: for the scenarios asserting that a configuration stops
    /// the engine at startup (feature 16's invalid route tables, feature 20's missing TLS file). An
    /// engine still running then is killed and reported with no exit code: it booted.
    pub async fn boot_engine(
        &self,
        flags: &[String],
        front_door: bool,
    ) -> Result<EngineBoot, HarnessError> {
        if self.config.mode != Mode::External {
            return Err(HarnessError::Unsupported {
                what: "a boot-only engine",
                mode: self.config.mode.name(),
            });
        }
        let opts = SpawnOptions {
            args: flags.to_vec(),
            front_door,
            ..SpawnOptions::default()
        };
        let (mut child, log) = spawn_engine(&self.config, &opts)?;
        let deadline = Instant::now() + READINESS_BUDGET;
        let exit_code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status.code().unwrap_or(-1)),
                Ok(None) if Instant::now() >= deadline => break None,
                Ok(None) => tokio::time::sleep(READINESS_SPACING).await,
                Err(source) => {
                    return Err(HarnessError::Io {
                        action: "waiting on the engine logging to",
                        path: log,
                        source,
                    });
                }
            }
        };
        if exit_code.is_none() {
            SpawnedEngine::from_child(child, log.clone()).stop().await?;
        }
        let output = std::fs::read_to_string(&log).map_err(|source| HarnessError::Io {
            action: "reading",
            path: log.clone(),
            source,
        })?;
        Ok(EngineBoot { exit_code, output })
    }

    /// Write `contents` to a fresh file in the run's log directory and return its path: a config
    /// file or a certificate an engine is started with.
    pub fn scratch_file(&self, stem: &str, contents: &str) -> Result<PathBuf, HarnessError> {
        let path = self.config.log_dir.join(format!(
            "{stem}-{}",
            SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        write_file(&path, contents)?;
        Ok(path)
    }

    /// Start a second, empty engine of the run's kind (feature 21 `@admin-isolated`), for admin
    /// calls that would wipe the shared engine: a spawned `rift`, or in embedded mode a second
    /// in-process engine. The caller owns it and stops it after the scenario.
    pub async fn start_isolated_engine(&self) -> Result<IsolatedEngine, HarnessError> {
        self.start_plain_engine(true).await
    }

    /// Start a second engine of the run's kind with no intercept listener (feature 12
    /// `@intercept-runtime`: the listener is started and stopped over its admin API), without
    /// `--allow-injection` unless `allow_injection`. The caller owns it and stops it after the
    /// scenario.
    pub async fn start_plain_engine(
        &self,
        allow_injection: bool,
    ) -> Result<IsolatedEngine, HarnessError> {
        let engine = match self.config.mode {
            Mode::External => {
                let opts = SpawnOptions {
                    no_injection: !allow_injection,
                    ..SpawnOptions::default()
                };
                OwnedEngine::Spawned(spawn_with_retry(&self.config, &opts).await?)
            }
            Mode::Embedded => {
                let opts = EmbeddedOptions {
                    no_injection: !allow_injection,
                    ..EmbeddedOptions::default()
                };
                OwnedEngine::Embedded(EmbeddedEngine::start(&self.config, &opts).await?)
            }
            Mode::Container => {
                return Err(HarnessError::Unsupported {
                    what: "the isolated admin engine",
                    mode: self.config.mode.name(),
                });
            }
        };
        let admin_url = match &engine {
            OwnedEngine::Spawned(e) => e.admin_url.clone(),
            OwnedEngine::Embedded(e) => e.admin_url.clone(),
        };
        Ok(IsolatedEngine { engine, admin_url })
    }

    /// Start a second engine of the run's kind whose intercept listener is seeded with the test CA,
    /// as the shared one's is (feature 12): requiring `Proxy-Authorization` for `auth` (`user:pass`,
    /// `@intercept-auth`), and trusting `upstream_ca` for outbound TLS (`@intercept-ws`, whose
    /// WebSocket relay dials a TLS origin that CA signed). The caller owns it.
    pub async fn start_intercept_engine(
        &self,
        auth: Option<&str>,
        upstream_ca: Option<&Path>,
    ) -> Result<InterceptEngine, HarnessError> {
        let (engine, intercept_port) = match self.config.mode {
            Mode::External => {
                let mut args = Vec::new();
                if let Some(credentials) = auth {
                    args.extend(["--intercept-auth".to_string(), credentials.to_string()]);
                }
                if let Some(ca) = upstream_ca {
                    args.extend(["--upstream-ca-file".to_string(), ca.display().to_string()]);
                }
                let opts = SpawnOptions {
                    args,
                    intercept: true,
                    ..SpawnOptions::default()
                };
                let e = spawn_with_retry(&self.config, &opts).await?;
                let port = e.intercept_port;
                (OwnedEngine::Spawned(e), port)
            }
            Mode::Embedded => {
                let opts = EmbeddedOptions {
                    upstream_ca_file: upstream_ca.map(Path::to_path_buf),
                    ..EmbeddedOptions::default()
                };
                let e = EmbeddedEngine::start(&self.config, &opts).await?;
                let e = e.with_test_ca_intercept(auth).await?;
                let port = e.intercept_port;
                (OwnedEngine::Embedded(e), port)
            }
            Mode::Container => {
                return Err(HarnessError::Unsupported {
                    what: "an intercept engine of a scenario's own",
                    mode: self.config.mode.name(),
                });
            }
        };
        let admin_url = match &engine {
            OwnedEngine::Spawned(e) => e.admin_url.clone(),
            OwnedEngine::Embedded(e) => e.admin_url.clone(),
        };
        let intercept_port = intercept_port.ok_or_else(|| {
            HarnessError::NotReady("the intercept engine has no intercept listener".to_string())
        })?;
        Ok(InterceptEngine {
            engine,
            admin_url,
            intercept_port,
        })
    }

    /// Start a dedicated engine serving `json` from a config file it reloads from (feature 11): a
    /// spawned `rift --configfile`, or in embedded mode a second in-process engine given the file as
    /// `configFile`. `front_door` also starts the front door, which only a spawned engine has. The
    /// caller owns it and stops it after the scenario.
    pub async fn start_reload_engine(
        &self,
        json: &str,
        front_door: bool,
    ) -> Result<ReloadEngine, HarnessError> {
        let config_path = self.config.log_dir.join("reload-config.json");
        write_file(&config_path, json)?;
        let (engine, admin_url) = match self.config.mode {
            Mode::External => {
                let opts = SpawnOptions {
                    args: vec![
                        "--configfile".to_string(),
                        config_path.display().to_string(),
                    ],
                    front_door,
                    ..SpawnOptions::default()
                };
                let e = spawn_with_retry(&self.config, &opts).await?;
                let url = e.admin_url.clone();
                (OwnedEngine::Spawned(e), url)
            }
            Mode::Embedded if !front_door => {
                let opts = EmbeddedOptions {
                    config_file: Some(config_path.clone()),
                    ..EmbeddedOptions::default()
                };
                let e = EmbeddedEngine::start(&self.config, &opts).await?;
                let url = e.admin_url.clone();
                (OwnedEngine::Embedded(e), url)
            }
            Mode::Embedded | Mode::Container => {
                return Err(HarnessError::Unsupported {
                    what: "the reload engine",
                    mode: self.config.mode.name(),
                });
            }
        };
        let front_door_port = match &engine {
            OwnedEngine::Spawned(e) => e.front_door_port,
            OwnedEngine::Embedded(_) => None,
        };
        Ok(ReloadEngine {
            engine,
            admin_url,
            config_path,
            front_door_port,
        })
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
            Some(Engine::Container(e)) => e.kill_now(),
            None => {}
        }
    }

    /// Stop the shared engine. Safe to call twice; the second call does nothing.
    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        let engine = self.engine.lock().await.take();
        match engine {
            Some(Engine::External(e)) => e.stop().await,
            Some(Engine::Embedded(e)) => e.stop().await,
            Some(Engine::Container(e)) => e.stop().await,
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
    /// `--front-door`'s port, when it was started with one.
    pub front_door_port: Option<u16>,
    /// The intercept listener's port, when it was started with one.
    pub intercept_port: Option<u16>,
    pub log: PathBuf,
}

/// How an engine of this run's own is started, beyond the flags every one gets.
#[derive(Debug, Clone, Default)]
struct SpawnOptions {
    /// Extra CLI arguments.
    args: Vec<String>,
    /// Extra environment variables.
    env: Vec<(String, String)>,
    /// Also start the front door, on a port the engine picks.
    front_door: bool,
    /// Also start an intercept listener seeded with the test CA, on a port the engine picks.
    intercept: bool,
    /// Leave out `--allow-injection`.
    no_injection: bool,
}

/// The committed test intercept CA's directory (rift-conformance#55).
fn test_ca_dir() -> PathBuf {
    crate::spec::fixtures_dir().join("intercept")
}

/// The test CA's certificate and key files.
fn test_ca_files() -> (PathBuf, PathBuf) {
    let dir = test_ca_dir();
    (dir.join("ca-cert.pem"), dir.join("ca-key.pem"))
}

/// How a boot-only engine ([`Harness::boot_engine`]) ended: its exit code (`None` when it was still
/// running, so it booted, and was killed) and everything it wrote.
#[derive(Debug, Clone)]
pub struct EngineBoot {
    pub exit_code: Option<i32>,
    pub output: String,
}

async fn spawn_with_retry(
    config: &Config,
    opts: &SpawnOptions,
) -> Result<SpawnedEngine, HarnessError> {
    let mut failures = Vec::new();
    for _ in 0..SPAWN_ATTEMPTS {
        match SpawnedEngine::start(config, opts).await {
            Ok(engine) => return Ok(engine),
            Err(e) => failures.push(e.to_string()),
        }
    }
    Err(HarnessError::NotReady(format!(
        "{SPAWN_ATTEMPTS} attempts failed:\n{}",
        failures.join("\n")
    )))
}

/// Names each spawned engine's log file, since its ports are not known until it has started.
static ENGINE_SEQ: AtomicUsize = AtomicUsize::new(0);
/// Names each [`Harness::scratch_file`].
static SCRATCH_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Spawn `rift --port 0 --nologfile --metrics-port 0 --allow-injection <args>` (plus `--front-door
/// 0` when asked) in the corpus root, output to a fresh log file, and return the child and that log.
/// Port 0 throughout: a probed port could be taken before the engine bound it, or answered by a
/// neighbour's loopback socket while the engine holds the wildcard (#1485).
fn spawn_engine(config: &Config, opts: &SpawnOptions) -> Result<(Child, PathBuf), HarnessError> {
    let front_door_args: &[&str] = if opts.front_door {
        &["--front-door", "0"]
    } else {
        &[]
    };
    let log = config.log_dir.join(format!(
        "engine-{}-{}.log",
        std::process::id(),
        ENGINE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
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
    let mut command = Command::new(&binary);
    command.args(["--port", "0", "--nologfile", "--metrics-port", "0"]);
    if !opts.no_injection {
        command.arg("--allow-injection");
    }
    if opts.intercept {
        let (cert, key) = test_ca_files();
        command
            .args(["--intercept-port", "0", "--intercept-ca-cert"])
            .arg(cert)
            .arg("--intercept-ca-key")
            .arg(key);
    }
    command
        .args(&opts.args)
        .args(front_door_args)
        .env("NO_COLOR", "1")
        .envs(opts.env.iter().map(|(k, v)| (k, v)));
    // A level set in this environment would also hide the info lines the ports are read from,
    // so they are added back to whatever filter it asks for.
    if let Some(filter) = listening_filter() {
        command.env("RUST_LOG", filter);
    }
    let child = command
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
    Ok((child, log))
}

/// The log targets of the three startup lines a spawned engine's ports are read from.
const LISTENING_TARGETS: [&str; 3] = [
    "rift_http_proxy::admin_api::server",
    "rift_http_proxy::server",
    "rift_http_proxy::front_door::listener",
];

impl SpawnedEngine {
    fn from_child(child: Child, log: PathBuf) -> Self {
        Self {
            child,
            admin_url: String::new(),
            metrics_url: String::new(),
            front_door_port: None,
            intercept_port: None,
            log,
        }
    }

    /// [`spawn_engine`], then read the ports it bound from its startup lines and wait until its
    /// admin and metrics listeners answer and the front door accepts.
    async fn start(config: &Config, opts: &SpawnOptions) -> Result<Self, HarnessError> {
        let (child, log) = spawn_engine(config, opts)?;
        let mut engine = Self::from_child(child, log);
        let ready = match engine.read_bound_ports(opts.front_door).await {
            Ok(()) => engine.await_ready().await,
            Err(e) => Err(e),
        };
        let ready = match ready {
            Ok(()) if opts.intercept => {
                let deadline = Instant::now() + READINESS_BUDGET;
                let mut liveness = Liveness::Child(&mut engine.child, &engine.log);
                match await_intercept(&engine.admin_url, &mut liveness, deadline).await {
                    Ok(port) => {
                        engine.intercept_port = Some(port);
                        await_tcp(port, &mut liveness, deadline).await
                    }
                    Err(e) => Err(e),
                }
            }
            other => other,
        };
        match ready {
            Ok(()) => Ok(engine),
            Err(e) => {
                let e = with_log(e, &engine.log);
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

    /// Fill in the admin and metrics URLs and the front door port from the engine's `listening on
    /// http://<addr>` startup lines, polling its log until they are all there. A not-ready error
    /// leaves the log out: the caller adds it once the engine is stopped.
    async fn read_bound_ports(&mut self, front_door: bool) -> Result<(), HarnessError> {
        let deadline = Instant::now() + READINESS_BUDGET;
        loop {
            // The file is created before the spawn, so a read failure is a real I/O error.
            let text = std::fs::read_to_string(&self.log).map_err(|source| HarnessError::Io {
                action: "reading",
                path: self.log.clone(),
                source,
            })?;
            let admin = listening_port(&text, "Admin API (Mountebank-compatible) listening on");
            let metrics = listening_port(&text, "Metrics server listening on");
            let door = listening_port(&text, "Front door listening on");
            if let (Some(admin), Some(metrics)) = (admin, metrics)
                && (door.is_some() || !front_door)
            {
                self.admin_url = format!("http://{LOOPBACK}:{admin}");
                self.metrics_url = format!("http://{LOOPBACK}:{metrics}");
                self.front_door_port = door.filter(|_| front_door);
                return Ok(());
            }
            if let Some(why) = Liveness::Child(&mut self.child, &self.log)
                .stopped()
                .await?
            {
                return Err(HarnessError::NotReady(format!(
                    "{why} before reporting the ports it bound"
                )));
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::NotReady(format!(
                    "the engine did not report the ports it bound within {READINESS_BUDGET:?}"
                )));
            }
            tokio::time::sleep(READINESS_SPACING).await;
        }
    }

    /// Admin and metrics listeners answer, and the front door accepts. A not-ready error leaves the
    /// log out: the caller adds it once the engine is stopped.
    async fn await_ready(&mut self) -> Result<(), HarnessError> {
        let deadline = Instant::now() + READINESS_BUDGET;
        await_listeners(
            &self.admin_url,
            &self.metrics_url,
            &mut Liveness::Child(&mut self.child, &self.log),
            deadline,
        )
        .await?;
        if let Some(port) = self.front_door_port {
            loop {
                let last = match tokio::net::TcpStream::connect((LOOPBACK, port)).await {
                    Ok(_) => break,
                    Err(e) => e.to_string(),
                };
                if Instant::now() >= deadline {
                    return Err(HarnessError::NotReady(format!(
                        "the front door on {port} did not accept within {READINESS_BUDGET:?} (last: {last})"
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

/// Whether the engine being probed for readiness still runs.
enum Liveness<'a> {
    Child(&'a mut Child, &'a Path),
    /// The container's name.
    Container(&'a str),
}

impl Liveness<'_> {
    /// `Some(why)` once the engine has stopped.
    async fn stopped(&mut self) -> Result<Option<String>, HarnessError> {
        match self {
            Self::Child(child, log) => match child.try_wait() {
                Ok(status) => Ok(status.map(|s| format!("rift exited ({s})"))),
                Err(source) => Err(HarnessError::Io {
                    action: "waiting on the engine logging to",
                    path: log.to_path_buf(),
                    source,
                }),
            },
            // An inspect that fails (the container is gone, the daemon is down) also means the
            // engine cannot answer, so it ends the wait with that error as the reason.
            Self::Container(name) => {
                let format = "{{.State.Running}} {{.State.ExitCode}} {{.State.OOMKilled}}";
                match docker(&["inspect", "-f", format, name], "inspect").await {
                    Ok(state) => {
                        let mut fields = state.split_whitespace();
                        match (fields.next(), fields.next(), fields.next()) {
                            (Some("true"), _, _) => Ok(None),
                            (_, code, oom) => Ok(Some(format!(
                                "container {name} exited (exit code {}, OOM-killed {})",
                                code.unwrap_or("?"),
                                oom.unwrap_or("?")
                            ))),
                        }
                    }
                    Err(e) => Err(HarnessError::NotReady(format!(
                        "could not inspect container {name}: {e}"
                    ))),
                }
            }
        }
    }
}

/// Poll the admin `/imposters` and then `/metrics` until each answers 200, failing fast once the
/// engine has stopped.
async fn await_listeners(
    admin_url: &str,
    metrics_url: &str,
    engine: &mut Liveness<'_>,
    deadline: Instant,
) -> Result<(), HarnessError> {
    let probe = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(HarnessError::Client)?;
    let urls = [
        format!("{admin_url}/imposters"),
        format!("{metrics_url}/metrics"),
    ];
    for url in &urls {
        loop {
            if let Some(why) = engine.stopped().await? {
                return Err(HarnessError::NotReady(format!(
                    "{why} before {url} answered"
                )));
            }
            let last = match probe.get(url).send().await {
                Ok(r) if r.status().is_success() => break,
                Ok(r) => format!("HTTP {}", r.status()),
                Err(e) => e.to_string(),
            };
            if Instant::now() >= deadline {
                return Err(HarnessError::NotReady(format!(
                    "{url} did not answer 200 within {READINESS_BUDGET:?} (last: {last})"
                )));
            }
            tokio::time::sleep(READINESS_SPACING).await;
        }
    }
    Ok(())
}

/// Poll `GET {admin}/intercept` until it answers 200, and return the `interceptPort` it reports: the
/// port the engine bound for an intercept listener asked for on port 0.
async fn await_intercept(
    admin_url: &str,
    engine: &mut Liveness<'_>,
    deadline: Instant,
) -> Result<u16, HarnessError> {
    let probe = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(HarnessError::Client)?;
    let url = format!("{admin_url}/intercept");
    loop {
        if let Some(why) = engine.stopped().await? {
            return Err(HarnessError::NotReady(format!(
                "{why} before its intercept listener ran"
            )));
        }
        let last = match probe.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.map_err(HarnessError::Client)?;
                return serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("interceptPort")?.as_u64())
                    .and_then(|p| u16::try_from(p).ok())
                    .ok_or_else(|| {
                        HarnessError::NotReady(format!("{url} reported no interceptPort: {body}"))
                    });
            }
            Ok(r) => format!("HTTP {}", r.status()),
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(HarnessError::NotReady(format!(
                "{url} did not answer 200 within {READINESS_BUDGET:?} (last: {last})"
            )));
        }
        tokio::time::sleep(READINESS_SPACING).await;
    }
}

/// Wait until `port` on the loopback accepts a TCP connection: a listener that answers no GET (the
/// intercept listener speaks CONNECT, the front door 404s an unrouted probe).
async fn await_tcp(
    port: u16,
    engine: &mut Liveness<'_>,
    deadline: Instant,
) -> Result<(), HarnessError> {
    loop {
        if let Some(why) = engine.stopped().await? {
            return Err(HarnessError::NotReady(format!(
                "{why} before port {port} accepted"
            )));
        }
        let last = match tokio::net::TcpStream::connect((LOOPBACK, port)).await {
            Ok(_) => return Ok(()),
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(HarnessError::NotReady(format!(
                "port {port} did not accept within {READINESS_BUDGET:?} (last: {last})"
            )));
        }
        tokio::time::sleep(READINESS_SPACING).await;
    }
}

/// The port in the first `<prefix> http://<host>:<port>[/...]` line of `log`, colour codes ignored.
fn listening_port(log: &str, prefix: &str) -> Option<u16> {
    let marker = format!("{prefix} http://");
    log.lines().find_map(|line| {
        let rest = strip_ansi(line).split(&marker).nth(1)?.to_string();
        let addr = rest.split(['/', ' ']).next()?;
        addr.rsplit(':').next()?.parse().ok()
    })
}

/// Drop `ESC [ ... <final byte>` colour sequences, in case `NO_COLOR` is not honoured.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The `RUST_LOG` a spawned engine gets when this environment sets a log level (`RUST_LOG`, or
/// `MB_LOGLEVEL`, which the engine reads as its `--loglevel`): that filter with the startup lines'
/// targets at `info` appended. `None` leaves the engine's default `info` level, which logs them.
fn listening_filter() -> Option<String> {
    let base = std::env::var("RUST_LOG")
        .ok()
        .or_else(|| std::env::var("MB_LOGLEVEL").ok())
        .filter(|v| !v.trim().is_empty())?;
    let targets = LISTENING_TARGETS.map(|t| format!("{t}=info")).join(",");
    Some(format!("{base},{targets}"))
}

/// A not-ready error with the engine log's tail appended.
fn with_log(e: HarnessError, log: &Path) -> HarnessError {
    match e {
        HarnessError::NotReady(why) => {
            HarnessError::NotReady(format!("{why}; log {}:\n{}", log.display(), log_tail(log)))
        }
        other => other,
    }
}

/// Run `docker <args>`, returning its trimmed stdout, or its stderr as the error.
async fn docker(args: &[&str], action: &'static str) -> Result<String, HarnessError> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| HarnessError::Io {
            action: "running",
            path: PathBuf::from("docker"),
            source,
        })?;
    if !out.status.success() {
        return Err(HarnessError::Docker {
            action,
            detail: format!(
                "{}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A `docker run` that lost a published port, most likely to a container an interrupted run leaked.
fn with_port_hint(e: HarnessError) -> HarnessError {
    match e {
        HarnessError::Docker { action, detail } if detail.contains("port is already allocated") => {
            HarnessError::Docker {
                action,
                detail: format!(
                    "{detail}\nhint: a container leaked by an interrupted run may hold it; \
                     look for one with `docker ps --filter label=rift-conformance`"
                ),
            }
        }
        other => other,
    }
}

/// `docker rm -f`'s complaint when the container is already gone, which is the goal state.
fn already_gone(e: &HarnessError) -> bool {
    matches!(e, HarnessError::Docker { detail, .. } if detail.contains("No such container"))
}

static CONTAINER_SEQ: AtomicUsize = AtomicUsize::new(0);

/// The engine image run with Docker (port of `RiftHarnessContainer.scala`).
#[derive(Debug)]
struct ContainerEngine {
    name: String,
    admin_url: String,
    metrics_url: String,
    /// The host port the intercept listener is published on.
    intercept_port: u16,
    log: PathBuf,
}

impl ContainerEngine {
    /// `docker run` the image with admin and metrics on free host ports, the imposter ranges
    /// published 1:1 and the corpus's `data/` mounted read-only, then wait until it answers. No
    /// `--rm`: an engine that exits early must keep its logs for the failure message, so every path
    /// out removes the container itself.
    async fn start(config: &Config, image: &str) -> Result<Self, HarnessError> {
        let admin_port = free_port()?;
        let metrics_port = free_port()?;
        let intercept_port = free_port()?;
        let name = format!(
            "rift-conf-{}-{}",
            std::process::id(),
            CONTAINER_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let log = config.log_dir.join(format!("engine-{admin_port}.log"));
        let mut args: Vec<String> = [
            "run",
            "-d",
            "--init",
            "--name",
            &name,
            "--label",
            CONTAINER_LABEL,
            // Loopback only: the admin API runs with --allow-injection.
            "-p",
            &format!("127.0.0.1:{admin_port}:{CONTAINER_ADMIN_PORT}"),
            "-p",
            &format!("127.0.0.1:{metrics_port}:{CONTAINER_METRICS_PORT}"),
            "-p",
            &format!("127.0.0.1:{intercept_port}:{CONTAINER_INTERCEPT_PORT}"),
        ]
        .map(String::from)
        .to_vec();
        for (lo, hi) in CONTAINER_IMPOSTER_RANGES {
            args.push("-p".to_string());
            args.push(format!("127.0.0.1:{lo}-{hi}:{lo}-{hi}"));
        }
        args.push("-v".to_string());
        args.push(format!(
            "{}:{IMAGE_WORKDIR}/data:ro",
            config.corpus_root.join("data").display()
        ));
        // Docker wants the bind source absolute, which the fixtures dir's `..` is not.
        args.push("-v".to_string());
        args.push(format!(
            "{}:{CONTAINER_CA_DIR}:ro",
            absolute(test_ca_dir())?.display()
        ));
        args.push(image.to_string());
        args.extend(
            [
                "--port",
                &CONTAINER_ADMIN_PORT.to_string(),
                "--allow-injection",
                "--metrics-port",
                &CONTAINER_METRICS_PORT.to_string(),
                "--intercept-port",
                &CONTAINER_INTERCEPT_PORT.to_string(),
                "--intercept-ca-cert",
                &format!("{CONTAINER_CA_DIR}/ca-cert.pem"),
                "--intercept-ca-key",
                &format!("{CONTAINER_CA_DIR}/ca-key.pem"),
            ]
            .map(String::from),
        );
        // A spawned engine reads RIFT_MAX_BODY_SIZE from the environment it inherits; the
        // container's environment is the image's, so the limit goes in as the flag.
        if let Some(max) = config.max_body_size {
            args.push("--max-body-size".to_string());
            args.push(max.to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let engine = Self {
            admin_url: format!("http://{LOOPBACK}:{admin_port}"),
            metrics_url: format!("http://{LOOPBACK}:{metrics_port}"),
            intercept_port,
            name,
            log,
        };
        if let Err(e) = docker(&refs, "run").await {
            // docker itself did not run (missing, or no daemon): nothing was created to remove.
            if matches!(e, HarnessError::Io { .. }) {
                return Err(e);
            }
            let e = with_port_hint(e);
            // A failed run can still have created the container (a port bind fails after create);
            // its stderr, in `e`, says more than its logs would.
            return Err(match engine.remove().await {
                Ok(()) => e,
                Err(stop) => HarnessError::NotReady(format!(
                    "{e}\nand removing that container then failed too: {stop}"
                )),
            });
        }
        let deadline = Instant::now() + READINESS_BUDGET;
        let mut liveness = Liveness::Container(&engine.name);
        let ready = match await_listeners(
            &engine.admin_url,
            &engine.metrics_url,
            &mut liveness,
            deadline,
        )
        .await
        {
            // The port the engine reports is the one inside the container; the host reaches the
            // published one.
            Ok(()) => match await_intercept(&engine.admin_url, &mut liveness, deadline).await {
                Ok(_) => await_tcp(engine.intercept_port, &mut liveness, deadline).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        match ready {
            Ok(()) => Ok(engine),
            Err(e) => {
                // Stopping writes the log the message quotes.
                let log = engine.log.clone();
                match engine.stop().await {
                    Ok(()) => Err(with_log(e, &log)),
                    Err(stop) => Err(HarnessError::NotReady(format!(
                        "{}\nand stopping that container then failed too: {stop}",
                        with_log(e, &log)
                    ))),
                }
            }
        }
    }

    /// `docker logs <name>` with stdout and stderr going to `log`.
    fn logs_command(&self) -> Result<std::process::Command, HarnessError> {
        let out = std::fs::File::create(&self.log).map_err(|source| HarnessError::Io {
            action: "creating",
            path: self.log.clone(),
            source,
        })?;
        let err = out.try_clone().map_err(|source| HarnessError::Io {
            action: "duplicating",
            path: self.log.clone(),
            source,
        })?;
        let mut cmd = std::process::Command::new("docker");
        cmd.args(["logs", &self.name])
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        Ok(cmd)
    }

    fn logs_written(
        &self,
        status: std::io::Result<std::process::ExitStatus>,
    ) -> Result<(), HarnessError> {
        let status = status.map_err(|source| HarnessError::Io {
            action: "running docker logs into",
            path: self.log.clone(),
            source,
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(HarnessError::Docker {
                action: "logs",
                detail: format!("{status}; its error is in {}", self.log.display()),
            })
        }
    }

    /// Remove the container, its logs written first. Both are attempted; a container that is
    /// already gone is the goal state.
    async fn stop(self) -> Result<(), HarnessError> {
        let logged = match self.logs_command() {
            Ok(cmd) => self.logs_written(Command::from(cmd).status().await),
            Err(e) => Err(e),
        };
        let removed = self.remove().await;
        match (logged, removed) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
            (Err(logs), Err(rm)) => Err(HarnessError::Docker {
                action: "rm -f",
                detail: format!("{rm}\nand writing its logs failed too: {logs}"),
            }),
        }
    }

    async fn remove(&self) -> Result<(), HarnessError> {
        match docker(&["rm", "-f", &self.name], "rm -f").await {
            Err(e) if !already_gone(&e) => Err(e),
            _ => Ok(()),
        }
    }

    /// The panic-path teardown: logs, then `docker rm -f`, both blocking.
    fn kill_now(self) {
        let logged = self
            .logs_command()
            .and_then(|mut cmd| self.logs_written(cmd.status()));
        if let Err(e) = logged {
            eprintln!("plane_a: writing container {} logs: {e}", self.name);
        }
        match std::process::Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdin(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => {}
            Ok(out) => eprintln!(
                "plane_a: docker rm -f {} ({}): {}",
                self.name,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => eprintln!("plane_a: running docker rm -f {}: {e}", self.name),
        }
    }
}

/// Container mode's fresh-imposter ports: a round-robin cursor over an inclusive range. Probing the
/// host cannot find a free one there (Docker's forwarder holds every published port), so a taken
/// port shows up as a refused POST and [`PortPool::claim`] moves on to the next.
#[derive(Debug)]
pub struct PortPool {
    lo: u16,
    /// `hi - lo`: the pool is `lo..=lo + span`.
    span: u16,
    cursor: AtomicUsize,
}

impl PortPool {
    /// The pool `lo..=hi`; just `lo` when `hi < lo`.
    #[must_use]
    pub fn new(lo: u16, hi: u16) -> Self {
        Self {
            lo,
            span: hi.saturating_sub(lo),
            cursor: AtomicUsize::new(0),
        }
    }

    /// The pool's lowest port.
    #[must_use]
    pub fn lo(&self) -> u16 {
        self.lo
    }

    #[must_use]
    pub fn size(&self) -> usize {
        usize::from(self.span) + 1
    }

    /// The next port, wrapping from the top of the range back to its bottom.
    pub fn next_port(&self) -> u16 {
        let offset = self.cursor.fetch_add(1, Ordering::Relaxed) % self.size();
        self.lo + u16::try_from(offset).unwrap_or(self.span)
    }

    /// Offer `post` successive pool ports until one is answered 201, at most once round the pool.
    /// `post` returns the status and body. Only a port the engine reports taken ([`port_taken`])
    /// moves on to the next; any other answer, and `post`'s own errors, end the walk at once.
    /// Returns the port and the 201's body.
    pub async fn claim<F, Fut>(&self, mut post: F) -> anyhow::Result<(u16, String)>
    where
        F: FnMut(u16) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<(u16, String)>>,
    {
        let mut first: Option<String> = None;
        for _ in 0..self.size() {
            let port = self.next_port();
            let (status, body) = post(port).await?;
            if status == 201 {
                return Ok((port, body));
            }
            if !port_taken(status, &body) {
                anyhow::bail!("port {port}: {status}, expected 201: {body}");
            }
            first.get_or_insert_with(|| format!("port {port}: {status}: {body}"));
        }
        anyhow::bail!(
            "all {} ports of the pool {}..={} were already in use; first refusal {}",
            self.size(),
            self.lo,
            self.lo + self.span,
            first.unwrap_or_default()
        )
    }
}

/// The engine refused an imposter because its port is taken: a 400 naming "already in use", whether
/// another imposter or another process holds it. A 500 is an engine fault, never a port to walk past.
#[must_use]
pub fn port_taken(status: u16, body: &str) -> bool {
    status == 400 && body.contains("already in use")
}

/// Feature 11's per-scenario engine and the config file it reloads from.
#[derive(Debug)]
pub struct ReloadEngine {
    engine: OwnedEngine,
    pub admin_url: String,
    pub config_path: PathBuf,
    pub front_door_port: Option<u16>,
}

/// An engine one scenario starts beside the shared one, of the run's own kind.
#[derive(Debug)]
enum OwnedEngine {
    Spawned(SpawnedEngine),
    Embedded(EmbeddedEngine),
}

impl OwnedEngine {
    /// The spawned engine's log; an embedded engine logs into this process.
    fn log(&self) -> Option<&Path> {
        match self {
            Self::Spawned(e) => Some(&e.log),
            Self::Embedded(_) => None,
        }
    }

    async fn stop(self) -> Result<(), HarnessError> {
        match self {
            Self::Spawned(e) => e.stop().await,
            Self::Embedded(e) => e.stop().await,
        }
    }
}

impl ReloadEngine {
    /// Replace the config file without reloading: the scenario drives the reload itself.
    pub fn rewrite_config(&self, json: &str) -> Result<(), HarnessError> {
        write_file(&self.config_path, json)
    }

    /// The spawned engine's log; an embedded engine logs into this process.
    #[must_use]
    pub fn log(&self) -> Option<&Path> {
        self.engine.log()
    }

    pub async fn stop(self) -> Result<(), HarnessError> {
        self.engine.stop().await
    }
}

/// Feature 21's `@admin-isolated` engine: an empty engine whose imposters `DELETE /imposters` may
/// wipe without touching the shared one's.
#[derive(Debug)]
pub struct IsolatedEngine {
    engine: OwnedEngine,
    pub admin_url: String,
}

impl IsolatedEngine {
    /// The spawned engine's log; an embedded engine logs into this process.
    #[must_use]
    pub fn log(&self) -> Option<&Path> {
        self.engine.log()
    }

    pub async fn stop(self) -> Result<(), HarnessError> {
        self.engine.stop().await
    }
}

/// Feature 12's intercept engine of a scenario's own: its admin API and intercept listener.
#[derive(Debug)]
pub struct InterceptEngine {
    engine: OwnedEngine,
    pub admin_url: String,
    pub intercept_port: u16,
}

impl InterceptEngine {
    /// The spawned engine's log; an embedded engine logs into this process.
    #[must_use]
    pub fn log(&self) -> Option<&Path> {
        self.engine.log()
    }

    pub async fn stop(self) -> Result<(), HarnessError> {
        self.engine.stop().await
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), HarnessError> {
    std::fs::write(path, contents).map_err(|source| HarnessError::Io {
        action: "writing",
        path: path.to_path_buf(),
        source,
    })
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
#[derive(Debug)]
struct EmbeddedEngine {
    handle: HandlePtr,
    admin_url: String,
    metrics_url: String,
    /// The intercept listener `rift_start_intercept` started, if one was.
    intercept_port: Option<u16>,
}

/// `rift_serve_admin` options beyond the ones every embedded engine gets.
#[derive(Debug, Default)]
struct EmbeddedOptions {
    /// Loaded, and the engine's reload source.
    config_file: Option<PathBuf>,
    /// `allowInjection: false`.
    no_injection: bool,
    /// `upstreamCaFile`: trusted for outbound TLS.
    upstream_ca_file: Option<PathBuf>,
}

/// `rift_start`'s opaque handle. Every C-ABI entry point takes it from any thread (the SDKs call
/// them from arbitrary JVM/Go/Node threads); it is only ever passed back to them.
#[derive(Debug)]
struct HandlePtr(*mut rift_ffi::RiftHandle);
// SAFETY: the handle is an opaque pointer into rift-ffi, whose entry points are documented as
// callable from any thread; this runner never dereferences it.
unsafe impl Send for HandlePtr {}

impl HandlePtr {
    /// Moving the whole wrapper into a closure (not its field) is what keeps it `Send`.
    fn into_raw(self) -> *mut rift_ffi::RiftHandle {
        self.0
    }

    /// The same handle again, for a call made while the engine owning it is still running and
    /// awaited before that engine can be stopped.
    fn alias(&self) -> Self {
        Self(self.0)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartInterceptReply {
    intercept_port: u16,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServeAdminReply {
    admin_url: String,
    metrics_port: Option<u16>,
}

impl EmbeddedEngine {
    async fn start(config: &Config, opts: &EmbeddedOptions) -> Result<Self, HarnessError> {
        let mut options = serde_json::json!({
            "host": "127.0.0.1",
            "port": 0,
            "metricsPort": 0,
            "allowInjection": !opts.no_injection,
        });
        if let Some(max) = config.max_body_size {
            options["maxBodySize"] = max.into();
        }
        if let Some(path) = &opts.config_file {
            options["configFile"] = path.display().to_string().into();
        }
        if let Some(path) = &opts.upstream_ca_file {
            options["upstreamCaFile"] = path.display().to_string().into();
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
            intercept_port: None,
        })
    }

    /// Start this engine's intercept listener with `rift_start_intercept`, on a loopback port it
    /// picks, seeded with the test CA, requiring `auth` (`user:pass`) when given. Its admin plane
    /// shares the listener, so the `/intercept*` admin routes see it. A failure stops the engine.
    async fn with_test_ca_intercept(mut self, auth: Option<&str>) -> Result<Self, HarnessError> {
        let (cert, key) = test_ca_files();
        let mut options = serde_json::json!({
            "host": "127.0.0.1",
            "port": 0,
            "caCertPath": cert.display().to_string(),
            "caKeyPath": key.display().to_string(),
        });
        if let Some(credentials) = auth {
            let (username, password) = credentials.split_once(':').ok_or_else(|| {
                HarnessError::Embedded(format!("intercept auth {credentials:?} is not user:pass"))
            })?;
            options["auth"] = serde_json::json!({"username": username, "password": password});
        }
        let started = match CString::new(options.to_string()) {
            Ok(options) => {
                let handle = self.handle.alias();
                tokio::task::spawn_blocking(move || {
                    // SAFETY: the handle is live (its engine is awaiting this task) and `options` is
                    // a NUL-terminated string that outlives the call.
                    let reply = unsafe {
                        rift_ffi::rift_start_intercept(handle.into_raw(), options.as_ptr())
                    };
                    if reply.is_null() {
                        return Err(format!("rift_start_intercept: {}", last_error()));
                    }
                    // SAFETY: a non-null reply is a NUL-terminated string rift-ffi allocated; it is
                    // copied here and then freed exactly once.
                    let text = unsafe { CStr::from_ptr(reply) }
                        .to_string_lossy()
                        .into_owned();
                    unsafe { rift_ffi::rift_free(reply) };
                    serde_json::from_str::<StartInterceptReply>(&text)
                        .map(|r| r.intercept_port)
                        .map_err(|e| format!("rift_start_intercept reply {text:?}: {e}"))
                })
                .await
                .unwrap_or_else(|e| Err(format!("rift_start_intercept task: {e}")))
            }
            Err(e) => Err(format!("intercept options JSON: {e}")),
        };
        match started {
            Ok(port) => {
                self.intercept_port = Some(port);
                Ok(self)
            }
            Err(why) => Err(with_stop_outcome(why, self.stop().await)),
        }
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
    fn only_embedded_excludes_the_spawned_engine_tags() {
        for tag in ["upstream-trust", "front-door"] {
            assert!(Mode::Embedded.excludes(tag).is_some(), "{tag}");
            assert!(Mode::External.excludes(tag).is_none(), "{tag}");
        }
        assert!(Mode::Embedded.excludes("full-corpus-verify").is_none());
        assert!(Mode::Embedded.excludes("metrics-port").is_none());
        assert!(Mode::Embedded.excludes("no-config-source").is_none());
    }

    #[test]
    fn debug_env_runs_only_in_external_mode_and_admin_isolated_not_in_a_container() {
        assert!(Mode::External.excludes("rift-debug-env").is_none());
        assert!(Mode::Embedded.excludes("rift-debug-env").is_some());
        assert!(Mode::Container.excludes("rift-debug-env").is_some());
        assert!(Mode::External.excludes("admin-isolated").is_none());
        assert!(Mode::Embedded.excludes("admin-isolated").is_none());
        assert!(Mode::Container.excludes("admin-isolated").is_some());
    }

    const CONTAINER_TAGS: [&str; 15] = [
        "rift-debug-env",
        "tls-flags",
        "intercept-runtime",
        "intercept-auth",
        "intercept-ws",
        "intercept-upstream-trust",
        "admin-isolated",
        "reload-engine",
        "upstream-trust",
        "front-door",
        "full-corpus-verify",
        "loopback-client",
        "connect-refused",
        "host-port",
        "tcp-rst",
    ];

    #[test]
    fn container_excludes_its_tags_and_the_other_modes_keep_theirs() {
        for tag in CONTAINER_TAGS {
            assert!(Mode::Container.excludes(tag).is_some(), "{tag}");
            assert!(Mode::External.excludes(tag).is_none(), "{tag}");
        }
        for tag in [
            "full-corpus-verify",
            "loopback-client",
            "connect-refused",
            "host-port",
            "tcp-rst",
        ] {
            assert!(Mode::Embedded.excludes(tag).is_none(), "{tag}");
        }
        for tag in ["full-corpus", "metrics-port", "no-config-source", "rift"] {
            assert!(Mode::Container.excludes(tag).is_none(), "{tag}");
        }
    }

    #[test]
    fn every_intercept_tag_runs_in_process_and_only_the_own_engine_ones_leave_the_container() {
        for tag in [
            "intercept-http",
            "intercept-caller-ca",
            "intercept-raw",
            "intercept-runtime",
            "intercept-auth",
            "intercept-ws",
            "intercept-upstream-trust",
        ] {
            assert!(Mode::External.excludes(tag).is_none(), "{tag}");
            assert!(Mode::Embedded.excludes(tag).is_none(), "{tag}");
        }
        for tag in [
            "intercept-http",
            "intercept-caller-ca",
            "intercept-raw",
            "intercept",
        ] {
            assert!(Mode::Container.excludes(tag).is_none(), "{tag}");
        }
    }

    #[test]
    fn tls_flag_engines_run_only_in_external_mode() {
        assert!(Mode::External.excludes("tls-flags").is_none());
        assert!(Mode::Embedded.excludes("tls-flags").is_some());
        assert!(Mode::Container.excludes("tls-flags").is_some());
        // Feature 20's untagged scenarios drive the shared engine in every mode.
        for mode in [Mode::External, Mode::Embedded, Mode::Container] {
            assert!(mode.excludes("tls").is_none(), "{mode}");
        }
    }

    #[test]
    fn only_container_excludes_the_reload_engine_scenarios() {
        assert!(Mode::Container.excludes("reload-engine").is_some());
        assert!(Mode::External.excludes("reload-engine").is_none());
        assert!(Mode::Embedded.excludes("reload-engine").is_none());
    }

    #[test]
    fn the_port_pool_wraps_from_its_top_to_its_bottom() {
        let pool = PortPool::new(CONTAINER_POOL.0, CONTAINER_POOL.1);
        assert_eq!(pool.size(), 20);
        let ports: Vec<u16> = (0..22).map(|_| pool.next_port()).collect();
        assert_eq!(ports[0], 4880);
        assert_eq!(ports[19], 4899);
        assert_eq!(ports[20], 4880);
        assert_eq!(ports[21], 4881);
    }

    fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
            .block_on(f)
    }

    #[test]
    fn claim_walks_past_refused_ports() {
        let pool = PortPool::new(4898, 4899);
        let _ = pool.next_port(); // the cursor now points at 4899, so the walk wraps.
        let mut offered = Vec::new();
        let claimed = block_on(pool.claim(|port| {
            offered.push(port);
            let reply = if port == 4898 {
                (201, "created".to_string())
            } else {
                (400, format!("Port {port} is already in use"))
            };
            async move { Ok(reply) }
        }))
        .expect("4898 is accepted");
        assert_eq!(claimed, (4898, "created".to_string()));
        assert_eq!(offered, [4899, 4898]);
    }

    #[test]
    fn claim_gives_up_after_one_round_with_the_first_refusal() {
        let pool = PortPool::new(4880, 4882);
        let mut tries = 0;
        let err = block_on(pool.claim(|port| {
            tries += 1;
            async move { Ok((400, format!("address already in use (port {port})"))) }
        }))
        .expect_err("every port refused");
        assert_eq!(tries, 3);
        let err = err.to_string();
        assert!(err.contains("all 3 ports"), "{err}");
        assert!(err.contains("(port 4880)"), "{err}");
    }

    #[test]
    fn claim_stops_at_a_refusal_that_is_not_a_taken_port() {
        let pool = PortPool::new(4880, 4899);
        let mut tries = 0;
        let err = block_on(pool.claim(|_| {
            tries += 1;
            async { Ok((400, "predicates must be an array".to_string())) }
        }))
        .expect_err("a validation error is final");
        assert_eq!(tries, 1);
        assert!(
            err.to_string().contains("predicates must be an array"),
            "{err}"
        );
    }

    #[test]
    fn only_a_refusal_naming_a_taken_port_is_retried() {
        assert!(port_taken(400, "Port 4880 is already in use"));
        assert!(port_taken(
            400,
            "bind: address already in use (os error 48)"
        ));
        assert!(!port_taken(500, "Port 4880 is already in use"));
        assert!(!port_taken(
            500,
            "Failed to bind port 4880: Address already in use (os error 48)"
        ));
        assert!(!port_taken(400, "invalid imposter"));
    }

    fn vars(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn container_mode_without_rift_image_is_a_configuration_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().display().to_string();
        let base = [
            ("RIFT_BUILD_DIR", path.clone()),
            ("RIFT_CORPUS_ROOT", path.clone()),
        ];
        let mut container = base.to_vec();
        container.push(("RIFT_MODE", "container".to_string()));
        std::fs::create_dir(dir.path().join("data")).expect("a data dir");
        match Config::from_vars(vars(&container), dir.path().to_path_buf()) {
            Err(HarnessError::MissingEnv { name, .. }) => assert_eq!(name, "RIFT_IMAGE"),
            other => panic!("expected MissingEnv RIFT_IMAGE, got {other:?}"),
        }
        container.push(("RIFT_IMAGE", "rift-proxy:local".to_string()));
        let config = Config::from_vars(vars(&container), dir.path().to_path_buf())
            .expect("valid")
            .expect("RIFT_BUILD_DIR is set");
        assert_eq!(config.image.as_deref(), Some("rift-proxy:local"));
        let external = Config::from_vars(vars(&base), dir.path().to_path_buf())
            .expect("valid")
            .expect("RIFT_BUILD_DIR is set");
        assert_eq!(external.mode, Mode::External);
        assert_eq!(external.image, None);
    }

    #[test]
    fn container_mode_without_a_corpus_data_dir_is_a_configuration_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().display().to_string();
        let container = [
            ("RIFT_BUILD_DIR", path.clone()),
            ("RIFT_CORPUS_ROOT", path),
            ("RIFT_MODE", "container".to_string()),
            ("RIFT_IMAGE", "rift-proxy:local".to_string()),
        ];
        match Config::from_vars(vars(&container), dir.path().to_path_buf()) {
            Err(HarnessError::InvalidEnv { name, .. }) => assert_eq!(name, "RIFT_CORPUS_ROOT"),
            other => panic!("expected InvalidEnv RIFT_CORPUS_ROOT, got {other:?}"),
        }
    }

    #[test]
    fn free_port_is_not_zero() {
        assert_ne!(free_port().expect("a free port"), 0);
    }

    #[test]
    fn listening_port_reads_each_startup_line() {
        let log = "\u{1b}[2m2026-10-16T00:00:00Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m rift_http_proxy::admin_api::server: \
                   Rift Admin API (Mountebank-compatible) listening on http://0.0.0.0:51234\n\
                   INFO rift_http_proxy::server: Metrics server listening on http://0.0.0.0:51235/metrics\n\
                   INFO rift_http_proxy::front_door::listener: Front door listening on http://0.0.0.0:51236\n";
        assert_eq!(
            listening_port(log, "Admin API (Mountebank-compatible) listening on"),
            Some(51234)
        );
        assert_eq!(
            listening_port(log, "Metrics server listening on"),
            Some(51235)
        );
        assert_eq!(listening_port(log, "Front door listening on"), Some(51236));
        assert_eq!(
            listening_port("starting up\n", "Front door listening on"),
            None
        );
    }
}
