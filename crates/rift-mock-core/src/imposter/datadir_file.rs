//! How an imposter's `--datadir` file is written (issue #1158).
//!
//! `{port}.json` used to be rewritten in place — open-with-truncate, then write — so a process that
//! died mid-write, a full disk, or a reload reading it at the wrong moment all saw an empty or
//! partial document, and the imposter's last good state was gone. It is now replaced: the new
//! document is written and synced to `{port}.json.tmp` beside it, then renamed over it, so a reader
//! or a crash only ever sees the old document or the new one.
//!
//! The temp name is fixed rather than random: every writer in a process is serialized by the
//! manager's `persist_lock` — held by the blocking write itself, so a dropped caller cannot release
//! it mid-write — and a fixed name keeps a crash's leftovers to one per port, overwritten by that
//! port's next write. Its extension is `tmp`, and both datadir loaders only read files whose
//! extension is `json`, so a leftover is never loaded.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// `{port}.json` — the complete persisted state of the imposter on `port`.
pub(crate) fn data_path(datadir: &Path, port: u16) -> PathBuf {
    datadir.join(format!("{port}.json"))
}

/// `{port}.json.tmp` — a write of `{port}.json` in progress, or one a crash interrupted.
pub(crate) fn temp_path(datadir: &Path, port: u16) -> PathBuf {
    datadir.join(format!("{port}.json.tmp"))
}

/// Replace `{port}.json` with `bytes`, so that a reader, or a crash, only ever sees the old document
/// or the new one.
///
/// The temp file is synced before the rename: without it, a power loss after the rename can leave
/// a zero-length `{port}.json` on a filesystem with delayed allocation — the same defect in another
/// form. The directory is not synced; that would only make the newest write durable, which was never
/// promised, and without it a power loss still leaves one complete document.
///
/// On failure `{port}.json` is untouched and the temp file is removed; the error is the write's own.
/// `persist` is the manager's `persist_lock`, released only when the rename is done.
pub(crate) async fn write_replacing(
    datadir: &Path,
    port: u16,
    bytes: Vec<u8>,
    persist: tokio::sync::OwnedMutexGuard<()>,
) -> io::Result<()> {
    let target = data_path(datadir, port);
    let temp = temp_path(datadir, port);
    tokio::task::spawn_blocking(move || {
        let _persist = persist;
        let written = write_and_sync(&temp, &bytes).and_then(|()| std::fs::rename(&temp, &target));
        if written.is_err() {
            // Terminal last-resort: the caller still gets the write's error, and a leftover temp
            // file is inert (never loaded, removed by the next start or this port's next write).
            match std::fs::remove_file(&temp) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(error = %e, ?temp, "could not remove a failed write's temp file")
                }
            }
        }
        written
    })
    .await
    .map_err(io::Error::other)?
}

fn write_and_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_data()
}

/// Remove every `{port}.json.tmp` in `datadir` — each one a write that a previous process did not
/// finish — and return their paths. `{port}.json` beside it is that imposter's last complete state.
///
/// Only names of exactly that shape are touched: an operator's `notes.json.tmp` is not rift's to
/// delete. Call it at startup, before any manager writes to `datadir`: run beside a live writer it
/// would unlink that writer's temp file and fail its rename.
pub fn sweep_interrupted_writes(datadir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for entry in std::fs::read_dir(datadir)? {
        let path = entry?.path();
        let is_ours = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json.tmp"))
            // Exactly the spelling rift writes: no sign, no leading zero.
            .is_some_and(|stem| {
                stem.parse::<u16>()
                    .is_ok_and(|port| port.to_string() == stem)
            });
        if !is_ours {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> tokio::sync::OwnedMutexGuard<()> {
        std::sync::Arc::new(tokio::sync::Mutex::new(()))
            .try_lock_owned()
            .expect("uncontended")
    }

    #[tokio::test]
    async fn a_write_replaces_the_file_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(data_path(dir.path(), 4545), "old").expect("seed");
        write_replacing(dir.path(), 4545, b"new".to_vec(), lock())
            .await
            .expect("write");
        assert_eq!(
            std::fs::read_to_string(data_path(dir.path(), 4545)).expect("read"),
            "new"
        );
        assert!(!temp_path(dir.path(), 4545).exists());
    }

    #[tokio::test]
    async fn a_failed_write_leaves_the_previous_file_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(data_path(dir.path(), 4545), "old").expect("seed");
        // A directory where the temp file must go makes the create fail.
        std::fs::create_dir(temp_path(dir.path(), 4545)).expect("block the temp path");
        write_replacing(dir.path(), 4545, b"new".to_vec(), lock())
            .await
            .expect_err("the temp file cannot be created");
        assert_eq!(
            std::fs::read_to_string(data_path(dir.path(), 4545)).expect("read"),
            "old"
        );
    }

    #[tokio::test]
    async fn a_failed_rename_removes_the_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A non-empty directory at the target makes the rename fail after the temp file is written.
        std::fs::create_dir(data_path(dir.path(), 4545)).expect("block the target");
        std::fs::write(data_path(dir.path(), 4545).join("x"), "").expect("fill it");
        write_replacing(dir.path(), 4545, b"new".to_vec(), lock())
            .await
            .expect_err("rename onto a non-empty directory fails");
        assert!(!temp_path(dir.path(), 4545).exists());
    }

    #[test]
    fn the_sweep_removes_only_port_named_temp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in [
            "4545.json.tmp",
            "65535.json.tmp",
            "4545.json",
            "notes.json.tmp",
            "65536.json.tmp",
            "+80.json.tmp",
            "04545.json.tmp",
            "4545.tmp",
        ] {
            std::fs::write(dir.path().join(name), "x").expect("seed");
        }
        let mut removed = sweep_interrupted_writes(dir.path()).expect("sweep");
        removed.sort();
        assert_eq!(
            removed,
            vec![
                dir.path().join("4545.json.tmp"),
                dir.path().join("65535.json.tmp")
            ]
        );
        for kept in [
            "4545.json",
            "notes.json.tmp",
            "65536.json.tmp",
            "+80.json.tmp",
            "04545.json.tmp",
            "4545.tmp",
        ] {
            assert!(dir.path().join(kept).exists(), "{kept} must be kept");
        }
    }
}
