//! Component registry admin handlers (issue #1757).
//!
//! `POST /components` uploads a WebAssembly component once, keyed by the SHA-256 of its bytes;
//! `_rift.script.component` then names it as `sha256:<hex>`. `GET` lists and reads, `DELETE`
//! removes one nothing references. With `--datadir` an upload is also kept in
//! `<datadir>/components/<hex>.wasm`, so it survives a restart.

use crate::admin_api::types::{collect_body, error_response, json_response};
use crate::imposter::components::{
    ComponentEntry, ComponentId, WORLD, config_scripts, stub_scripts,
};
use crate::imposter::{ImposterConfig, ImposterError, ImposterManager, RiftScriptConfig, Stub};
use crate::response::{ErrorKind, error_response_typed};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// The upload's only accepted media type.
const WASM_CONTENT_TYPE: &str = "application/wasm";

/// The largest component one upload may carry (issue #1757). A registry entry is held for the
/// process's life, compiled on upload, and kept as base64, so it is bounded on its own rather than
/// only by the admin body limit.
pub(crate) const MAX_COMPONENT_BYTES: usize = 64 << 20;
/// The most component bytes the registry holds, all uploads together: past it an upload answers
/// `507` rather than growing the process until it is killed.
pub(crate) const MAX_REGISTRY_BYTES: usize = 512 << 20;

/// Held across a registry change and its datadir file, so an upload and a delete of one component
/// cannot interleave into a registry and a datadir that disagree.
static WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn bad_data(message: &str) -> Response<Full<Bytes>> {
    error_response_typed(StatusCode::BAD_REQUEST, ErrorKind::BadData, message)
}

/// RFC 3339, UTC, millisecond precision.
fn rfc3339(at: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(at).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn upload_json(entry: &ComponentEntry) -> serde_json::Value {
    serde_json::json!({
        "sha256": entry.id.hex(),
        "size": entry.size,
        "world": WORLD,
        "imports": entry.imports,
    })
}

fn detail_json(entry: &ComponentEntry, manager: &ImposterManager) -> serde_json::Value {
    let mut value = upload_json(entry);
    value["uploadedAt"] = rfc3339(entry.uploaded_at).into();
    value["referencedBy"] = manager.component_references(&entry.id).into();
    value
}

/// The `Content-Type` is `application/wasm`, parameters and case aside.
fn is_wasm_content_type(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case(WASM_CONTENT_TYPE))
}

/// `<datadir>/components`.
pub(crate) fn components_dir(datadir: &Path) -> PathBuf {
    datadir.join("components")
}

fn component_file(datadir: &Path, id: &ComponentId) -> PathBuf {
    components_dir(datadir).join(format!("{}.wasm", id.hex()))
}

/// Write `bytes` to its file: a temp file in the same directory, renamed into place, so a reader
/// never sees half of one.
fn write_component_file(datadir: &Path, id: &ComponentId, bytes: &[u8]) -> std::io::Result<()> {
    let dir = components_dir(datadir);
    std::fs::create_dir_all(&dir)?;
    let target = component_file(datadir, id);
    let temp = dir.join(format!("{}.wasm.tmp", id.hex()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, &target).inspect_err(|_| {
        // Best effort: the rename's own error is the one reported.
        let _ = std::fs::remove_file(&temp);
    })
}

/// Remove a component's file; one that is not there is already removed.
fn remove_component_file(datadir: &Path, id: &ComponentId) -> std::io::Result<()> {
    match std::fs::remove_file(component_file(datadir, id)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A `<datadir>/components/*.wasm` that was not loaded, and why.
#[derive(Debug)]
pub(crate) struct SkippedComponentFile {
    pub(crate) path: PathBuf,
    pub(crate) reason: String,
}

/// Register every `<datadir>/components/*.wasm` with `manager`. A file whose bytes do not hash to
/// its name, or that the manager refuses (not a component, not in `--strict-wasm`), is skipped and
/// returned. A missing directory is no components; one that cannot be read is a skipped entry.
pub(crate) fn load_stored_components(
    manager: &ImposterManager,
    datadir: &Path,
) -> Vec<SkippedComponentFile> {
    let dir = components_dir(datadir);
    let mut skipped = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return skipped,
        Err(e) => {
            skipped.push(SkippedComponentFile {
                path: dir,
                reason: format!("could not read the components directory: {e}"),
            });
            return skipped;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                skipped.push(SkippedComponentFile {
                    path: dir.clone(),
                    reason: format!("could not read a directory entry: {e}"),
                });
                continue;
            }
        };
        if path.extension().is_none_or(|e| e != "wasm") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                skipped.push(SkippedComponentFile {
                    path,
                    reason: format!("could not read file: {e}"),
                });
                continue;
            }
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        let actual = ComponentId::of(&bytes);
        if actual.hex() != stem {
            skipped.push(SkippedComponentFile {
                path,
                reason: format!(
                    "the file's contents hash to {actual}, not the sha256:{stem} its name claims"
                ),
            });
            continue;
        }
        if let Err(e) = manager.upload_component(&bytes) {
            skipped.push(SkippedComponentFile {
                path,
                reason: format!("component refused: {e}"),
            });
        }
    }
    skipped
}

fn dangling<'a>(
    scripts: impl IntoIterator<Item = &'a RiftScriptConfig>,
    manager: &ImposterManager,
) -> Option<Response<Full<Bytes>>> {
    for script in scripts {
        let Some(reference) = script.component.as_deref() else {
            continue;
        };
        let message = match reference.parse::<ComponentId>() {
            Err(e) => e.to_string(),
            Ok(id) if manager.components().get(&id).is_none() => {
                format!("unknown component {id}; upload it with POST /components")
            }
            Ok(_) => continue,
        };
        return Some(bad_data(&message));
    }
    None
}

/// Refuse a `_rift.script.component` among `stubs` that is not an uploaded component, as 400 `bad
/// data`. Admin doors only: a stored config replayed at startup, `--configfile` or reload loads
/// with the reference dangling and fails when served (rift-cluster replays stored configs).
pub(crate) fn reject_dangling_in_stubs(
    stubs: &[Stub],
    manager: &ImposterManager,
) -> Option<Response<Full<Bytes>>> {
    dangling(stub_scripts(stubs), manager)
}

/// [`reject_dangling_in_stubs`] over a whole imposter: its stubs and its `_rift.scripts` registry.
pub(crate) fn reject_dangling_in_config(
    config: &ImposterConfig,
    manager: &ImposterManager,
) -> Option<Response<Full<Bytes>>> {
    dangling(config_scripts(config), manager)
}

/// POST /components
pub async fn handle_upload(
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
    datadir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    if !is_wasm_content_type(&req) {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/wasm",
        );
    }
    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    if body.bytes().len() > MAX_COMPONENT_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!(
                "a component may be at most {MAX_COMPONENT_BYTES} bytes (got {})",
                body.bytes().len()
            ),
        );
    }
    let _writes = WRITES.lock().await;
    let held = manager
        .components()
        .get(&rift_mock_core::imposter::components::ComponentId::of(
            body.bytes(),
        ))
        .is_some();
    if !held && manager.components().total_bytes() + body.bytes().len() > MAX_REGISTRY_BYTES {
        return error_response(
            StatusCode::INSUFFICIENT_STORAGE,
            &format!(
                "the component registry holds at most {MAX_REGISTRY_BYTES} bytes; delete unused \
                 components first"
            ),
        );
    }
    let (entry, new) = match manager.upload_component(&body) {
        Ok(uploaded) => uploaded,
        Err(ImposterError::InvalidConfig(message)) => return bad_data(&message),
        Err(e) => return e.into(),
    };
    if new && let Some(datadir) = datadir {
        let (dir, id, bytes) = (Arc::clone(&datadir), entry.id, body.bytes().clone());
        let written = tokio::task::spawn_blocking(move || write_component_file(&dir, &id, &bytes))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        if let Err(e) = written {
            // Held in memory but not kept: undo, so the answer matches what a restart would find.
            manager.components().remove(&entry.id);
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("could not save the component to the datadir: {e}"),
            );
        }
    }
    json_response(
        if new {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        &upload_json(&entry),
    )
}

/// GET /components
pub async fn handle_list(manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    let components: Vec<_> = manager
        .components()
        .list()
        .iter()
        .map(|entry| detail_json(entry, &manager))
        .collect();
    json_response(
        StatusCode::OK,
        &serde_json::json!({ "components": components }),
    )
}

fn parse_id(sha: &str) -> Result<ComponentId, String> {
    ComponentId::from_hex(sha)
        .map_err(|_| format!("component id must be 64 lowercase hex digits (got {sha:?})"))
}

fn not_found(id: &ComponentId) -> Response<Full<Bytes>> {
    error_response(StatusCode::NOT_FOUND, &format!("component {id} not found"))
}

/// GET /components/:sha
pub async fn handle_get(sha: &str, manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    let id = match parse_id(sha) {
        Ok(id) => id,
        Err(message) => return bad_data(&message),
    };
    match manager.components().get(&id) {
        Some(entry) => json_response(StatusCode::OK, &detail_json(&entry, &manager)),
        None => not_found(&id),
    }
}

/// DELETE /components/:sha
pub async fn handle_delete(
    sha: &str,
    manager: Arc<ImposterManager>,
    datadir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let id = match parse_id(sha) {
        Ok(id) => id,
        Err(message) => return bad_data(&message),
    };
    let _writes = WRITES.lock().await;
    if manager.components().get(&id).is_none() {
        return not_found(&id);
    }
    let ports = manager.component_references(&id);
    if !ports.is_empty() {
        let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
        return error_response(
            StatusCode::CONFLICT,
            &format!("component is referenced by imposters {}", ports.join(", ")),
        );
    }
    if let Some(datadir) = datadir {
        let dir = Arc::clone(&datadir);
        let removed = tokio::task::spawn_blocking(move || remove_component_file(&dir, &id))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        if let Err(e) = removed {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("could not remove the component's file from the datadir: {e}"),
            );
        }
    }
    manager.components().remove(&id);
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_files_that_do_not_load_are_reported_by_path_and_reason() {
        let datadir = tempfile::tempdir().expect("tempdir");
        let dir = components_dir(datadir.path());
        std::fs::create_dir_all(&dir).expect("mkdir");
        let bytes = b"not a component";
        // Right name, not a component.
        let named = dir.join(format!("{}.wasm", ComponentId::of(bytes).hex()));
        std::fs::write(&named, bytes).expect("write");
        // Wrong name.
        let forged = dir.join(format!("{}.wasm", "e".repeat(64)));
        std::fs::write(&forged, bytes).expect("write");
        // Not a component file at all.
        std::fs::write(dir.join("notes.txt"), "x").expect("write");

        let skipped = load_stored_components(&ImposterManager::new(), datadir.path());
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        let reason = |path: &Path| {
            skipped
                .iter()
                .find(|s| s.path == path)
                .map(|s| s.reason.clone())
                .unwrap_or_default()
        };
        assert!(reason(&named).contains("component refused"), "{skipped:?}");
        assert!(reason(&forged).contains("hash to"), "{skipped:?}");
    }

    #[test]
    fn no_components_directory_is_no_components() {
        let datadir = tempfile::tempdir().expect("tempdir");
        assert!(load_stored_components(&ImposterManager::new(), datadir.path()).is_empty());
    }
}
