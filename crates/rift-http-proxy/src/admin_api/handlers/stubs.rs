//! Stub management handlers.

use crate::admin_api::handlers::imposters::handle_get as handle_get_imposter;
use crate::admin_api::handlers::imposters::{
    admin_script_access, admin_script_base, imposter_default_script_engine,
    imposter_script_registry, imposter_script_timeout_ms, reject_failing_generators,
    reject_stubs_if_injection_disallowed, reject_unreadable_lookups,
};
use crate::admin_api::types::{
    AddStubRequest, ReplaceStubsRequest, StubWithLinks, collect_body, error_response,
    json_response, make_stub_links,
};
use crate::extensions::stub_analysis::analyze_new_stub;
use crate::imposter::{ImposterError, ImposterManager, Stub, resolve_stub_scripts};
use crate::response::{ErrorKind, error_response_typed};
use crate::scripting::{validate_stub, validate_stubs};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

/// Resolve `_rift.script` `file:`/`ref:` sources in admin-API-supplied stubs at WRITE time (issue
/// #356 B1), under the same `--scripts-dir` root as whole-imposter create, before the stubs are
/// persisted. Returns a `400` on any escape / unknown-ref / unconfigured-`file:` error so nothing
/// unresolved (`file`/`ref` still set) is ever written to the datadir, and on a `lookup` CSV path
/// the imposter may not read (issue #1617). `None` on success.
fn resolve_admin_stubs(
    stubs: &mut [Stub],
    manager: &ImposterManager,
    port: u16,
    scripts_dir: &Option<Arc<PathBuf>>,
) -> Option<Response<Full<Bytes>>> {
    let registry = imposter_script_registry(manager, port);
    let default_engine = imposter_default_script_engine(manager, port);
    let base = admin_script_base(scripts_dir);
    match resolve_stub_scripts(stubs, &registry, &default_engine, &base) {
        Ok(()) => reject_unreadable_lookups(stubs, scripts_dir),
        Err(e) => Some(error_response(
            StatusCode::BAD_REQUEST,
            &format!("Script resolution failed: {e}"),
        )),
    }
}

/// POST /imposters/:port/stubs - Add a stub
pub async fn handle_add(
    port: u16,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    let mut add_req: AddStubRequest = match super::decode_config_body(body.bytes().clone()).await {
        Ok(r) => r,
        Err(e) => return e.into_response("Invalid stub JSON"),
    };

    // Gate any scripting surface behind --allowInjection before mutating state (B3, issue #355).
    if let Some(rejection) =
        reject_stubs_if_injection_disallowed(std::slice::from_ref(&add_req.stub), allow_injection)
    {
        return rejection;
    }
    // After the gate, before anything is stored (issue #1580).
    if let Some(rejection) = reject_failing_generators(
        std::slice::from_ref(&add_req.stub),
        imposter_script_timeout_ms(&manager, port),
        admin_script_access(&scripts_dir),
    )
    .await
    {
        return rejection;
    }

    // Resolve `_rift.script` `file:`/`ref:` sources before persisting (issue #356 B1): an escape /
    // unknown-ref / unconfigured `file:` is a 400, and nothing unresolved is ever stored.
    if let Some(rejection) = resolve_admin_stubs(
        std::slice::from_mut(&mut add_req.stub),
        &manager,
        port,
        &scripts_dir,
    ) {
        return rejection;
    }

    // Issue #202: honor a caller-supplied `id`, but generate a stable one if absent so every
    // stub is addressable via the by-id endpoints.
    if add_req.stub.id.is_none() {
        add_req.stub.id = Some(uuid::Uuid::new_v4().to_string());
    }

    // Validate scripts in the stub before adding
    let insert_index = add_req.index.unwrap_or(0);
    let validation_result = validate_stub(&add_req.stub, insert_index);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }

    // Analyze the new stub against existing stubs (Rift extension)
    if let Ok(imposter) = manager.get_imposter(port) {
        let existing_stubs = imposter.get_stubs();
        let insert_index = add_req.index.unwrap_or(existing_stubs.len());

        // Reject out-of-bounds index before mutating state
        if insert_index > existing_stubs.len() {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!(
                    "Stub index {insert_index} is out of range (imposter has {} stubs)",
                    existing_stubs.len()
                ),
            );
        }

        let analysis = analyze_new_stub(&existing_stubs, &add_req.stub, insert_index);

        for warning in &analysis.warnings {
            warn!(
                port = port,
                stub_id = ?add_req.stub.id,
                warning_type = ?warning.warning_type,
                "New stub warning: {}",
                warning.message
            );
        }
    }

    match manager
        .add_stub_from(
            port,
            add_req.stub,
            add_req.index,
            admin_script_access(&scripts_dir),
        )
        .await
    {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}

/// PUT /imposters/:port/stubs - Replace all stubs
pub async fn handle_replace_all(
    port: u16,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    let mut replace_req: ReplaceStubsRequest =
        match super::decode_config_body(body.bytes().clone()).await {
            Ok(r) => r,
            Err(e) => return e.into_response("Invalid stubs JSON"),
        };

    // Gate any scripting surface behind --allowInjection before mutating state (B3, issue #355).
    if let Some(rejection) =
        reject_stubs_if_injection_disallowed(&replace_req.stubs, allow_injection)
    {
        return rejection;
    }
    // After the gate, before anything is stored (issue #1580).
    if let Some(rejection) = reject_failing_generators(
        &replace_req.stubs,
        imposter_script_timeout_ms(&manager, port),
        admin_script_access(&scripts_dir),
    )
    .await
    {
        return rejection;
    }

    // Resolve `_rift.script` `file:`/`ref:` sources before persisting (issue #356 B1).
    if let Some(rejection) =
        resolve_admin_stubs(&mut replace_req.stubs, &manager, port, &scripts_dir)
    {
        return rejection;
    }

    // Validate all scripts in stubs before replacing
    let validation_result = validate_stubs(&replace_req.stubs);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }

    // Stub-overlap analysis is computed once in core on the replace (issue #423) and surfaced by
    // the GET render below via the imposter's cached warnings — no redundant recompute here.
    if let Err(e) = manager
        .replace_stubs_from(port, replace_req.stubs, admin_script_access(&scripts_dir))
        .await
    {
        return e.into();
    }

    handle_get_imposter(port, None, base_url, manager).await
}

/// GET /imposters/:port/stubs - Get all stubs
pub async fn handle_get_all(
    port: u16,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let stubs = imposter.get_stubs();
            let stubs_with_links: Vec<StubWithLinks> = stubs
                .into_iter()
                .enumerate()
                .map(|(index, stub)| StubWithLinks {
                    stub,
                    links: make_stub_links(base_url, port, index),
                })
                .collect();
            json_response(
                StatusCode::OK,
                &serde_json::json!({ "stubs": stubs_with_links }),
            )
        }
        Err(e) => e.into(),
    }
}

/// Mountebank's answer to a stub index that is not a current array position, verbatim
/// (`validateStubIndex`, `imposterController.js`). `PUT` and `DELETE` share the text.
const STUB_INDEX_MESSAGE: &str = "'stubIndex' must be a valid integer, representing the array index position of the stub to replace";

/// A stub index as Mountebank reads one: the canonical decimal of an existing position (issue
/// #1552). Mountebank looks the raw path string up in the stub array, so only `"0"`, `"1"`, … up
/// to the last position name a stub: `01`, `+1`, `-0`, `1.0`, an overflow and an out-of-range
/// number do not. Its `length` quirk (the array's own property) is deliberately not reproduced.
fn stub_position(raw: &str, len: usize) -> Option<usize> {
    // Domain-optional parse: a segment that is not a decimal names no position.
    let n: usize = raw.parse().ok()?;
    (n.to_string() == raw && n < len).then_some(n)
}

/// Mountebank's `404` `bad data` for an index that names no current position.
fn bad_stub_index() -> Response<Full<Bytes>> {
    error_response_typed(
        StatusCode::NOT_FOUND,
        ErrorKind::BadData,
        STUB_INDEX_MESSAGE,
    )
}

/// Why an index-addressed request names no stub.
#[derive(Debug)]
enum IndexRefusal {
    /// The imposter is absent: it keeps its own answer.
    Imposter(ImposterError),
    /// The index is not a current position.
    NotAPosition,
}

impl From<IndexRefusal> for Response<Full<Bytes>> {
    fn from(refusal: IndexRefusal) -> Self {
        match refusal {
            IndexRefusal::Imposter(e) => e.into(),
            IndexRefusal::NotAPosition => bad_stub_index(),
        }
    }
}

/// Resolves `raw` against the imposter's current stubs, imposter first, as Mountebank checks:
/// an absent imposter keeps its own answer, then an index that names no position is refused.
fn resolve_stub_index(
    manager: &ImposterManager,
    port: u16,
    raw: &str,
) -> Result<usize, IndexRefusal> {
    let imposter = manager.get_imposter(port).map_err(IndexRefusal::Imposter)?;
    stub_position(raw, imposter.stub_count()).ok_or(IndexRefusal::NotAPosition)
}

/// An error from an index-addressed manager call. The index was resolved a moment earlier, so
/// `StubIndexOutOfBounds` here means the stubs shrank in between: it is answered as the bad index
/// it now is. Only these handlers do so; the shared mapping stays (`move_stub` and embedders).
fn stub_index_error(err: ImposterError) -> Response<Full<Bytes>> {
    match err {
        ImposterError::StubIndexOutOfBounds(_) => bad_stub_index(),
        other => other.into(),
    }
}

/// GET /imposters/:port/stubs/:index - Get a specific stub
pub async fn handle_get(
    port: u16,
    raw_index: &str,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let index = match resolve_stub_index(&manager, port, raw_index) {
        Ok(index) => index,
        Err(refusal) => return refusal.into(),
    };
    match manager.get_stub(port, index) {
        Ok(stub) => {
            let stub_with_links = StubWithLinks {
                stub,
                links: make_stub_links(base_url, port, index),
            };
            json_response(StatusCode::OK, &stub_with_links)
        }
        Err(e) => stub_index_error(e),
    }
}

/// PUT /imposters/:port/stubs/:index - Replace a specific stub
pub async fn handle_replace(
    port: u16,
    raw_index: &str,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    // The index is checked before the body is read, as Mountebank checks it (issue #1552).
    let index = match resolve_stub_index(&manager, port, raw_index) {
        Ok(index) => index,
        Err(refusal) => return refusal.into(),
    };

    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    let mut stub: Stub = match super::decode_config_body(body.bytes().clone()).await {
        Ok(s) => s,
        Err(e) => return e.into_response("Invalid stub JSON"),
    };

    // Gate any scripting surface behind --allowInjection before mutating state (B3, issue #355).
    if let Some(rejection) =
        reject_stubs_if_injection_disallowed(std::slice::from_ref(&stub), allow_injection)
    {
        return rejection;
    }
    // After the gate, before anything is stored (issue #1580).
    if let Some(rejection) = reject_failing_generators(
        std::slice::from_ref(&stub),
        imposter_script_timeout_ms(&manager, port),
        admin_script_access(&scripts_dir),
    )
    .await
    {
        return rejection;
    }

    // Resolve `_rift.script` `file:`/`ref:` sources before persisting (issue #356 B1).
    if let Some(rejection) = resolve_admin_stubs(
        std::slice::from_mut(&mut stub),
        &manager,
        port,
        &scripts_dir,
    ) {
        return rejection;
    }

    // Validate scripts in the stub before replacing
    let validation_result = validate_stub(&stub, index);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }

    match manager
        .replace_stub_from(port, index, stub, admin_script_access(&scripts_dir))
        .await
    {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => stub_index_error(e),
    }
}

/// DELETE /imposters/:port/stubs/:index - Delete a specific stub
pub async fn handle_delete(
    port: u16,
    raw_index: &str,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let index = match resolve_stub_index(&manager, port, raw_index) {
        Ok(index) => index,
        Err(refusal) => return refusal.into(),
    };
    match manager.delete_stub(port, index).await {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => stub_index_error(e),
    }
}

// ── Id-addressed stub operations (issue #202) ───────────────────────────────────

/// GET /imposters/:port/stubs/by-id/:id — fetch the stub addressed by id.
pub async fn handle_get_by_id(
    port: u16,
    id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_stub_by_id(port, id) {
        Ok(stub) => json_response(StatusCode::OK, &stub),
        Err(e) => e.into(),
    }
}

/// PUT /imposters/:port/stubs/by-id/:id — replace the stub addressed by id, in place.
pub async fn handle_replace_by_id(
    port: u16,
    id: &str,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let mut stub: Stub = match super::decode_config_body(body.bytes().clone()).await {
        Ok(s) => s,
        Err(e) => return e.into_response("Invalid stub JSON"),
    };
    // Gate any scripting surface behind --allowInjection before mutating state (B3, issue #355).
    if let Some(rejection) =
        reject_stubs_if_injection_disallowed(std::slice::from_ref(&stub), allow_injection)
    {
        return rejection;
    }
    // After the gate, before anything is stored (issue #1580).
    if let Some(rejection) = reject_failing_generators(
        std::slice::from_ref(&stub),
        imposter_script_timeout_ms(&manager, port),
        admin_script_access(&scripts_dir),
    )
    .await
    {
        return rejection;
    }
    // Resolve `_rift.script` `file:`/`ref:` sources before persisting (issue #356 B1).
    if let Some(rejection) = resolve_admin_stubs(
        std::slice::from_mut(&mut stub),
        &manager,
        port,
        &scripts_dir,
    ) {
        return rejection;
    }
    let validation_result = validate_stub(&stub, 0);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }
    match manager
        .replace_stub_by_id_from(port, id, stub, admin_script_access(&scripts_dir))
        .await
    {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/stubs/by-id/:id — delete the stub addressed by id.
pub async fn handle_delete_by_id(
    port: u16,
    id: &str,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.delete_stub_by_id(port, id).await {
        Ok(()) => handle_get_imposter(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::stub_position;

    /// Issue #1552: only the canonical decimal of an existing position names a stub.
    #[test]
    fn stub_position_accepts_only_a_canonical_existing_position() {
        assert_eq!(stub_position("0", 2), Some(0));
        assert_eq!(stub_position("1", 2), Some(1));
        for raw in [
            "2",
            "5",
            "01",
            "00",
            "+1",
            "-0",
            "-1",
            "1.0",
            " 1",
            "abc",
            "length",
            "",
            "18446744073709551616",
        ] {
            assert_eq!(stub_position(raw, 2), None, "{raw:?}");
        }
        assert_eq!(stub_position("0", 0), None);
    }
}
