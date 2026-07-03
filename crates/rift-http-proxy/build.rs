//! Stamp build identity (issue #344) into compile-time env vars. Mirrors `rift-ffi/build.rs` so
//! the admin `GET /config` `commit` field (process mode) reports the same identity as
//! `rift_build_info` (FFI mode) — one version-coherence preflight works for every mode.
//!
//! Each value: an explicit env override wins (CI, source tarballs), else it is derived here, else
//! left unset so `option_env!` is `None` → JSON `null`.

use std::process::Command;

fn main() {
    if let Some(commit) = env_or("RIFT_COMMIT", &["git", "rev-parse", "HEAD"]) {
        println!("cargo:rustc-env=RIFT_COMMIT={commit}");
    }
    if let Some(built_at) = env_or("RIFT_BUILT_AT", &["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"]) {
        println!("cargo:rustc-env=RIFT_BUILT_AT={built_at}");
    }
    println!("cargo:rerun-if-env-changed=RIFT_COMMIT");
    println!("cargo:rerun-if-env-changed=RIFT_BUILT_AT");
    stamp_reruns_on_head_move();
}

/// The value of env var `key` if set, else the trimmed stdout of `cmd` (if it runs and succeeds
/// with non-empty output), else `None`.
fn env_or(key: &str, cmd: &[&str]) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        return Some(v);
    }
    // A spawn failure is the expected "tool absent" case (source tarball, no git) → silent None.
    let output = Command::new(cmd[0]).args(&cmd[1..]).output().ok()?;
    if !output.status.success() {
        // Tool present but failed (e.g. a corrupt repo) is a real problem, not the tarball case —
        // surface it rather than silently stamping null.
        println!(
            "cargo:warning={key}: `{}` failed: {}",
            cmd.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Re-run the build script when HEAD moves, so the stamped commit isn't frozen across incremental
/// rebuilds. `HEAD` catches branch switches and the reflog (`logs/HEAD`) catches new commits;
/// both resolve correctly inside a git worktree via `git rev-parse --git-path`. Only existing
/// paths are emitted — a missing `rerun-if-changed` path forces an unconditional rebuild.
fn stamp_reruns_on_head_move() {
    for rel in ["HEAD", "logs/HEAD"] {
        let Ok(output) = Command::new("git")
            .args(["rev-parse", "--git-path", rel])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        if let Ok(path) = String::from_utf8(output.stdout) {
            let path = path.trim();
            if !path.is_empty() && std::path::Path::new(path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
}
