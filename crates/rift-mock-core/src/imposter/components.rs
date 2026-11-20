//! The content-addressed component registry (issue #1757): wasm components uploaded with
//! `POST /components`, referenced from `_rift.script` as `component: "sha256:<hex>"`.
//!
//! An upload is validated exactly as admitting a `code`/`file` component is (#1751: a component,
//! importing only what Rift links, exporting `rift:mock/handler`), so a bad one is refused at upload
//! with the import or export named. The registry is held by the [`ImposterManager`] and read when a
//! stub is served: a reference this registry does not hold is a script error then, never a load
//! failure, because a replayed config (rift-cluster's log, `--datadir`) must load on a node whose
//! registry is empty.
//!
//! The registry keeps no files. Persistence under `--datadir` is the admin server's, which writes
//! each upload and loads the directory at startup.
//!
//! [`ImposterManager`]: crate::imposter::ImposterManager

use crate::imposter::ImposterConfig;
use crate::imposter::types::{RiftScriptConfig, StubResponse};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

/// A component's identity: the SHA-256 of its bytes. Written `sha256:<64 lowercase hex>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId([u8; 32]);

impl ComponentId {
    /// The id of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
        let mut id = [0u8; 32];
        id.copy_from_slice(digest.as_ref());
        Self(id)
    }

    /// The 64 lowercase hex digits, without the `sha256:` prefix.
    #[must_use]
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parse 64 lowercase hex digits, without the prefix: a `--strict-wasm` line, a file name.
    pub fn from_hex(hex: &str) -> Result<Self, ComponentIdError> {
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ComponentIdError(hex.to_owned()));
        }
        let mut id = [0u8; 32];
        for (i, byte) in id.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                .map_err(|_| ComponentIdError(hex.to_owned()))?;
        }
        Ok(Self(id))
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex())
    }
}

impl fmt::Debug for ComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A `component` value that is not `sha256:` and 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("component must be \"sha256:\" and 64 lowercase hex digits (got {0:?})")]
pub struct ComponentIdError(pub String);

impl FromStr for ComponentId {
    type Err = ComponentIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.strip_prefix("sha256:")
            .ok_or_else(|| ComponentIdError(s.to_owned()))
            .and_then(Self::from_hex)
            .map_err(|_| ComponentIdError(s.to_owned()))
    }
}

/// The world every registered component implements.
pub const WORLD: &str = "rift:mock/mock";

/// One registered component.
#[derive(Debug)]
pub struct ComponentEntry {
    pub id: ComponentId,
    /// The bytes as base64: the form `_rift.script.code` carries, which the engine's compile cache
    /// is keyed by, so an upload warms it for the first request.
    pub code: Arc<str>,
    /// The bytes' length.
    pub size: usize,
    /// What the component imports, by interface name.
    pub imports: Vec<String>,
    pub uploaded_at: SystemTime,
}

/// Why an upload was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComponentError {
    /// Not a component Rift can run: #1751's admission message.
    #[error("{0}")]
    Invalid(String),
}

/// The components this process holds, by id.
#[derive(Debug, Default)]
pub struct ComponentRegistry {
    entries: RwLock<HashMap<ComponentId, Arc<ComponentEntry>>>,
}

impl ComponentRegistry {
    /// Validate `bytes` and hold them. Returns the entry and whether it is new; the same bytes
    /// again return the entry already held, unchanged.
    pub fn insert(&self, bytes: &[u8]) -> Result<(Arc<ComponentEntry>, bool), ComponentError> {
        let id = ComponentId::of(bytes);
        if let Some(held) = self.get(&id) {
            return Ok((held, false));
        }
        let code = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let imports = validate(&code)?;
        let entry = Arc::new(ComponentEntry {
            id,
            code: Arc::from(code),
            size: bytes.len(),
            imports,
            uploaded_at: SystemTime::now(),
        });
        // Two concurrent uploads of the same bytes: the first insert wins, and both answer it.
        let mut entries = self.entries.write();
        let held = entries.entry(id).or_insert_with(|| Arc::clone(&entry));
        let new = Arc::ptr_eq(held, &entry);
        Ok((Arc::clone(held), new))
    }

    #[must_use]
    pub fn get(&self, id: &ComponentId) -> Option<Arc<ComponentEntry>> {
        self.entries.read().get(id).cloned()
    }

    /// Every entry, by id.
    #[must_use]
    pub fn list(&self) -> Vec<Arc<ComponentEntry>> {
        let mut all: Vec<_> = self.entries.read().values().cloned().collect();
        all.sort_by_key(|e| e.id);
        all
    }

    /// The bytes held, all components together.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.entries.read().values().map(|e| e.size).sum()
    }

    /// Drop `id`; `false` if it was not held. Whether an imposter still references it is the
    /// caller's check ([`ImposterManager::component_references`]).
    ///
    /// [`ImposterManager::component_references`]: crate::imposter::ImposterManager::component_references
    pub fn remove(&self, id: &ComponentId) -> bool {
        self.entries.write().remove(id).is_some()
    }
}

#[cfg(feature = "wasm")]
fn validate(code: &str) -> Result<Vec<String>, ComponentError> {
    crate::scripting::wasm_admit_imports(code).map_err(ComponentError::Invalid)
}

#[cfg(not(feature = "wasm"))]
fn validate(_code: &str) -> Result<Vec<String>, ComponentError> {
    Err(ComponentError::Invalid(
        crate::scripting::WASM_UNAVAILABLE.to_owned(),
    ))
}

fn response_script(response: &StubResponse) -> Option<&RiftScriptConfig> {
    match response {
        StubResponse::Is {
            rift: Some(rift), ..
        } => rift.script.as_ref(),
        StubResponse::RiftScript { rift, .. } => rift.script.as_ref(),
        _ => None,
    }
}

/// Every `component` value `config` names: its stubs' responses and its `_rift.scripts` entries.
pub fn references(config: &ImposterConfig) -> impl Iterator<Item = &str> {
    config_scripts(config).filter_map(|script| script.component.as_deref())
}

/// `--strict-wasm` (issue #1757): the components an operator trusts, by hash. When set, a wasm
/// component whose hash is not listed is refused when it is admitted, uploaded or not. Approve the
/// behaviour keyed by the hash; you never have to judge whether the code is safe to run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedComponents(std::collections::HashSet<ComponentId>);

impl TrustedComponents {
    /// One lowercase SHA-256 (64 hex digits) per line; blank lines and `#` comments are ignored.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut ids = std::collections::HashSet::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let id = ComponentId::from_hex(line).map_err(|_| {
                format!(
                    "line {}: expected 64 lowercase hex digits (a SHA-256), got {line:?}",
                    n + 1
                )
            })?;
            ids.insert(id);
        }
        Ok(Self(ids))
    }

    #[must_use]
    pub fn contains(&self, id: &ComponentId) -> bool {
        self.0.contains(id)
    }

    /// Refuse `id` unless it is listed.
    pub fn check(&self, id: &ComponentId) -> Result<(), String> {
        if self.contains(id) {
            Ok(())
        } else {
            Err(format!("component {id} is not in --strict-wasm"))
        }
    }

    /// The first wasm component among `scripts` that is not listed, as the refusal message. A
    /// script is a component when its engine (or, engine-less, `default_engine`) is wasm, or it
    /// names a `component`. Fails closed: a component whose hash cannot be taken (`code` that is not
    /// base64, a malformed `component`) is refused.
    pub fn first_untrusted<'a>(
        &self,
        scripts: impl IntoIterator<Item = &'a RiftScriptConfig>,
        default_engine: &str,
    ) -> Option<String> {
        scripts.into_iter().find_map(|script| {
            let id = if let Some(reference) = script.component.as_deref() {
                match reference.parse::<ComponentId>() {
                    Ok(id) => id,
                    Err(e) => return Some(e.to_string()),
                }
            } else {
                let code = script.code.as_deref()?;
                use base64::Engine as _;
                let decoded = base64::engine::general_purpose::STANDARD.decode(code.trim());
                let wasm = match script.engine.as_deref() {
                    Some(engine) => crate::scripting::ScriptEngineKind::is_wasm(engine),
                    // Engine-less: what the default says, and also any code that *is* wasm, so a
                    // default changed by a concurrent replace cannot make a component skip the
                    // list (the check and the write read the imposter at different moments).
                    None => {
                        crate::scripting::ScriptEngineKind::is_wasm(default_engine)
                            || decoded.as_deref().is_ok_and(|b| b.starts_with(b"\0asm"))
                    }
                };
                if !wasm {
                    return None;
                }
                match decoded {
                    Ok(bytes) => ComponentId::of(&bytes),
                    Err(_) => {
                        return Some("component is not base64; cannot check --strict-wasm".into());
                    }
                }
            };
            self.check(&id).err()
        })
    }
}

/// Every `_rift.script` in `stubs`' responses.
pub fn stub_scripts(stubs: &[crate::imposter::Stub]) -> impl Iterator<Item = &RiftScriptConfig> {
    stubs
        .iter()
        .flat_map(|stub| response_scripts(&stub.responses))
}

/// Every `_rift.script` in `responses`.
pub fn response_scripts(responses: &[StubResponse]) -> impl Iterator<Item = &RiftScriptConfig> {
    responses.iter().filter_map(response_script)
}

/// Every `_rift.script` in `config`: its stubs' and its `_rift.scripts` entries.
pub fn config_scripts(config: &ImposterConfig) -> impl Iterator<Item = &RiftScriptConfig> {
    stub_scripts(&config.stubs).chain(config.rift.iter().flat_map(|rift| rift.scripts.values()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_round_trips_and_refuses_anything_else() {
        let id = ComponentId::of(b"abc");
        assert_eq!(
            id.to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(id.to_string().parse::<ComponentId>(), Ok(id));
        assert_eq!(ComponentId::from_hex(&id.hex()), Ok(id));
        for bad in [
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "sha256:ba78",
            "sha512:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256:za7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ] {
            assert!(bad.parse::<ComponentId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_trust_list_parses_and_checks() {
        let id = ComponentId::of(b"abc");
        let trusted = TrustedComponents::parse(&format!(
            "# approved\n\n{}  # the abc component\n",
            id.hex()
        ))
        .expect("parses");
        assert!(trusted.check(&id).is_ok());
        let other = ComponentId::of(b"abd");
        assert_eq!(
            trusted.check(&other),
            Err(format!("component {other} is not in --strict-wasm"))
        );
        let err = TrustedComponents::parse("abc\n").expect_err("not a hash");
        assert!(err.starts_with("line 1:"), "{err}");
    }

    #[test]
    fn first_untrusted_hashes_code_and_reads_references_and_fails_closed() {
        use base64::Engine as _;
        let listed = ComponentId::of(b"listed");
        let trusted = TrustedComponents::parse(&listed.hex()).expect("parses");
        let script = |v: serde_json::Value| -> RiftScriptConfig {
            serde_json::from_value(v).expect("script")
        };
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let ok = [
            script(serde_json::json!({ "engine": "wasm", "code": b64(b"listed") })),
            script(serde_json::json!({ "component": listed.to_string() })),
            // Not wasm: not this list's business.
            script(serde_json::json!({ "engine": "rhai", "code": "fn respond(ctx) { pass() }" })),
        ];
        assert_eq!(trusted.first_untrusted(&ok, "rhai"), None);
        let unlisted = ComponentId::of(b"other");
        for (bad, why) in [
            (
                script(serde_json::json!({ "engine": "wasm", "code": b64(b"other") })),
                unlisted.to_string(),
            ),
            (
                script(serde_json::json!({ "component": unlisted.to_string() })),
                unlisted.to_string(),
            ),
            // Engine-less wasm bytes are one under any default (see the rhai pass below).
            (
                script(serde_json::json!({ "code": b64(b"\0asm\x0d\0\x01\0") })),
                "is not in".to_owned(),
            ),
            // Engine-less code under a wasm default is a component.
            (
                script(serde_json::json!({ "code": b64(b"other") })),
                unlisted.to_string(),
            ),
            (
                script(serde_json::json!({ "engine": "wasm", "code": "not base64!" })),
                "not base64".into(),
            ),
            (
                script(serde_json::json!({ "component": "sha256:nope" })),
                "sha256:".into(),
            ),
        ] {
            let refusal = trusted.first_untrusted([&bad], "wasm").expect("refused");
            assert!(refusal.contains(&why), "{refusal} lacks {why}");
        }
        let wasm_bytes = script(serde_json::json!({ "code": b64(b"\0asm\x0d\0\x01\0") }));
        assert!(
            trusted.first_untrusted([&wasm_bytes], "rhai").is_some(),
            "wasm bytes under a rhai default are still checked"
        );
    }

    #[test]
    fn a_config_names_its_references() {
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "_rift": { "scripts": { "r": { "component": "sha256:aa" } } },
            "stubs": [{ "responses": [
                { "_rift": { "script": { "component": "sha256:bb" } } },
                { "is": { "statusCode": 200 } }
            ] }]
        }))
        .expect("config");
        let mut refs: Vec<&str> = references(&config).collect();
        refs.sort_unstable();
        assert_eq!(refs, ["sha256:aa", "sha256:bb"]);
    }

    #[cfg(not(feature = "wasm"))]
    #[test]
    fn without_the_engine_an_upload_is_refused() {
        let registry = ComponentRegistry::default();
        assert!(matches!(
            registry.insert(b"\0asm"),
            Err(ComponentError::Invalid(_))
        ));
        assert!(registry.list().is_empty());
    }
}
