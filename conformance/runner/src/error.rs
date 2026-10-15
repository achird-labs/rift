//! The harness's error type: everything that can go wrong before a step's own assertion runs.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("{name} must be set: {why}")]
    MissingEnv {
        name: &'static str,
        why: &'static str,
    },

    #[error("{name}={value:?} is invalid: {why}")]
    InvalidEnv {
        name: &'static str,
        value: String,
        why: String,
    },

    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed BUILD_INFO at {path}: {source}")]
    BuildInfo {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("could not reserve a free local port: {0}")]
    NoFreePort(#[source] std::io::Error),

    #[error("engine not ready: {0}")]
    NotReady(String),

    #[error("engine (pid {pid}) survived SIGKILL for 5s")]
    SurvivedTeardown { pid: u32 },

    #[error("{what} needs a spawned engine; RIFT_MODE={mode} cannot start one")]
    Unsupported {
        what: &'static str,
        mode: &'static str,
    },

    #[error("docker {action}: {detail}")]
    Docker {
        action: &'static str,
        detail: String,
    },

    #[error("embedded engine: {0}")]
    Embedded(String),

    #[error(
        "{tool} output is not the expected JSON ({source}); exit {exit}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    ToolOutput {
        tool: &'static str,
        exit: i32,
        stdout: String,
        stderr: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("{tool} was killed by a signal\nstderr:\n{stderr}")]
    ToolKilled { tool: &'static str, stderr: String },

    #[error("building an HTTP client: {0}")]
    Client(#[source] reqwest::Error),
}
