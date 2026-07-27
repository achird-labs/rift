//! Pluggable admin-API authorization (issue #854).
//!
//! The built-in admin gate is a single global api key: success yields *access*, not an identity.
//! Every caller is equivalent, so an embedder with its own identity system has to reverse-proxy
//! the admin API and re-parse routes just to recover enough context to make a decision — exactly
//! the drift-prone part.
//!
//! [`AdminAuthorizer`] is consulted **after** the route has been parsed, with the route class,
//! port and path params already extracted. Install nothing and nothing changes: with no authorizer
//! registered the api-key comparison decides on its own, exactly as before.
//!
//! # Ordering is part of the contract
//!
//! Authentication runs first and unconditionally; only then is the route parsed and this hook
//! consulted. That ordering is load-bearing: if authentication moved after route parsing, an
//! unauthenticated request to an unknown path would answer `404` instead of `401`, turning
//! unknown-path responses into an unauthenticated route-existence oracle.
//!
//! `Deny` on an authenticated request is a `403`. `401` stays reserved for a missing or malformed
//! credential.

use std::future::Future;
use std::sync::Arc;

/// What the caller is trying to do, as the admin router understood it.
///
/// Borrows from the request, so an authorizer sees the credential and path params without this
/// type having to allocate on a hot path.
///
/// `Debug` is hand-written to redact [`credential`](Self::credential): it is the verbatim admin
/// token, and a derived `Debug` would put it in any embedder's log the first time someone writes
/// `tracing::debug!("{req:?}")`. Same rule the CA key material follows in `intercept_control`.
#[derive(Clone, Copy)]
pub struct AuthzRequest<'a> {
    /// The `Authorization` header value, verbatim — `None` when the header is absent.
    ///
    /// Passed through untouched: upstream neither parses nor validates the scheme, because an
    /// embedder's credential format (bearer JWT, mTLS-derived, opaque session) is not upstream's
    /// vocabulary.
    pub credential: Option<&'a str>,
    /// A stable action string such as `"imposter.write"`.
    ///
    /// Deliberately a string and not an enum: an enum would force every embedder's action
    /// vocabulary to be upstream's, and adding an action would be a breaking change. See
    /// [`actions`] for the ones upstream emits.
    pub action: &'static str,
    /// The imposter port the route targets, when the route has one. `None` for collection routes
    /// (`POST /imposters` has no port yet) and for system routes.
    pub port: Option<u16>,
    /// The flow/space identifier, for the correlated-isolation routes.
    pub space: Option<&'a str>,
    /// Embedder-defined scope selector, verbatim from the request.
    ///
    /// Upstream neither parses nor interprets it. An authorizer often cannot derive the target
    /// from [`port`](Self::port) alone — `POST /imposters` creates a port rather than naming one —
    /// so this is the escape hatch for saying *which* tenant/scope a create belongs to.
    ///
    /// **It is caller-asserted and must never be trusted as identity.** It arrives in a request
    /// header, so any authenticated caller can set it to any value. Cross-check it against what
    /// [`credential`](Self::credential) actually entitles the caller to; using it directly as the
    /// authorization subject authorizes the caller's own claim about themselves.
    pub scope: Option<&'a str>,
    /// Route path parameters already parsed by the router, as `(name, value)`.
    ///
    /// Opaque to upstream beyond having been extracted. Without these an authorizer cannot make a
    /// decision about routes keyed by a path param rather than by port (a stub id, a scenario
    /// name, a flow-state key).
    pub params: &'a [(&'a str, &'a str)],
}

impl std::fmt::Debug for AuthzRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthzRequest")
            .field("credential", &self.credential.map(|_| "<redacted>"))
            .field("action", &self.action)
            .field("port", &self.port)
            .field("space", &self.space)
            .field("scope", &self.scope)
            .field("params", &self.params)
            .finish()
    }
}

/// The authorizer's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDecision {
    /// Let the request through, optionally attributing it to a principal.
    ///
    /// `principal` is the attribution handoff: it is what reaches change events so an audit trail
    /// can say *who* changed something and not only *what* changed.
    Allow { principal: Option<String> },
    /// Refuse the request. Answers `403` with the standard error envelope; `reason` is surfaced
    /// as the message, so it must not carry anything the caller should not see.
    Deny { reason: &'static str },
}

impl AuthzDecision {
    /// Allow with no attribution — what the built-in api-key gate returns.
    #[must_use]
    pub fn allow() -> Self {
        Self::Allow { principal: None }
    }

    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

/// The action strings upstream emits, as constants so an embedder can match without retyping
/// them and a rename here becomes a compile error there rather than a silently-never-matching
/// arm.
///
/// The mapping is mechanical — resource plus verb class — so it stays predictable as routes are
/// added, with one deliberate exception noted on [`IMPOSTER_VERIFY`].
pub mod actions {
    /// `GET /`, `/health`, `/config`, `/logs`, `/metrics`.
    pub const SYSTEM_READ: &str = "system.read";
    /// `POST /admin/reload`.
    pub const SYSTEM_WRITE: &str = "system.write";
    /// Any `GET` under `/imposters`, including stubs, saved requests and scenarios.
    pub const IMPOSTER_READ: &str = "imposter.read";
    /// Any `POST`/`PUT` that mutates an imposter or its stubs, scenarios or flow state.
    pub const IMPOSTER_WRITE: &str = "imposter.write";
    /// Any `DELETE` under `/imposters`, including `DELETE /imposters` (delete-all).
    ///
    /// Also covers `PUT /imposters`, which is destructive despite its method: it reconciles the
    /// imposter set toward the payload, so an empty list removes everything. A principal granted
    /// [`IMPOSTER_WRITE`] but not this cannot reach it.
    pub const IMPOSTER_DELETE: &str = "imposter.delete";
    /// `POST /imposters/:port/verify`.
    ///
    /// A `POST` that mutates nothing — it asserts against already-recorded requests. Mapping it
    /// to [`IMPOSTER_WRITE`] purely because of its method would stop a read-only principal
    /// verifying, so it gets its own string rather than a wrong one.
    pub const IMPOSTER_VERIFY: &str = "imposter.verify";
    /// `GET /events` — the cross-imposter SSE stream.
    ///
    /// Distinct from [`IMPOSTER_READ`] because it is not scoped to a port: it carries recorded
    /// requests from *every* imposter, so granting a principal read on one port must not
    /// implicitly grant this.
    pub const EVENTS_READ: &str = "events.read";
    /// `GET` under `/intercept`.
    pub const INTERCEPT_READ: &str = "intercept.read";
    /// `POST`/`PUT`/`DELETE` under `/intercept` — lifecycle, rules and CA.
    pub const INTERCEPT_WRITE: &str = "intercept.write";
}

/// Decides whether an authenticated admin request may proceed.
///
/// Registered with `ServerBuilder::admin_authorizer`. Implementations must be cheap and must not
/// block: this runs inline on every admin request, before the handler.
pub trait AdminAuthorizer: Send + Sync {
    fn authorize(&self, req: AuthzRequest<'_>) -> AuthzDecision;
}

/// An authorizer that allows everything, attributing nothing.
///
/// Not installed by default — the default is *no* authorizer at all, which skips the hook
/// entirely. This exists so an embedder can wrap or test against a known-inert baseline.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl AdminAuthorizer for AllowAll {
    fn authorize(&self, _req: AuthzRequest<'_>) -> AuthzDecision {
        AuthzDecision::allow()
    }
}

/// Convenience alias for the registration type.
pub type SharedAdminAuthorizer = Arc<dyn AdminAuthorizer>;

tokio::task_local! {
    /// Attribution for the admin request the current task is serving (issue #855).
    static PRINCIPAL: Option<String>;
}

/// Run `fut` with `principal` as the ambient attribution for change events (issue #855).
///
/// The principal is decided at the admin listener, but it has to reach `ImposterManager::emit`,
/// which sits several layers down in `rift-mock-core` behind manager methods that take no
/// principal. Threading a parameter through every one of those — `create_imposter`,
/// `delete_imposter`, `apply_config`, `add_stub`, … — would be a large breaking API change for one
/// optional feature, so this follows the seam the codebase already uses for exactly this shape:
/// `with_annotation_scope` in [`crate::extensions::decorate`]. Task-locals follow the task across
/// `.await`s, so a synchronous `emit` anywhere inside the request lands in this scope.
///
/// **Boundary:** `tokio::spawn` starts a task that does *not* inherit this scope. Every current
/// emit site is a direct call inside the mutating manager method, so all of them are covered; an
/// emit moved into a spawned task would silently attribute `None`.
pub async fn with_principal_scope<F: Future>(principal: Option<String>, fut: F) -> F::Output {
    PRINCIPAL.scope(principal, fut).await
}

/// The principal attributed to the current request, or `None` outside any request scope.
#[must_use]
pub fn current_principal() -> Option<String> {
    // Domain-optional read, not a swallowed error: `try_with` fails precisely when no scope is
    // open, which is the legitimate "no request behind this change" case (config-file load, an
    // embedder calling the manager directly). There is nothing to report and nothing to fail.
    PRINCIPAL.try_with(Clone::clone).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_helper_carries_no_principal() {
        assert_eq!(
            AuthzDecision::allow(),
            AuthzDecision::Allow { principal: None }
        );
    }

    #[test]
    fn deny_is_not_allowed() {
        assert!(!AuthzDecision::Deny { reason: "nope" }.is_allowed());
        assert!(AuthzDecision::allow().is_allowed());
        assert!(
            AuthzDecision::Allow {
                principal: Some("alice".into())
            }
            .is_allowed()
        );
    }

    #[test]
    fn allow_all_admits_every_action() {
        let a = AllowAll;
        for action in [
            actions::IMPOSTER_WRITE,
            actions::SYSTEM_READ,
            actions::IMPOSTER_DELETE,
        ] {
            let decision = a.authorize(AuthzRequest {
                credential: None,
                action,
                port: None,
                space: None,
                scope: None,
                params: &[],
            });
            assert_eq!(decision, AuthzDecision::allow());
        }
    }

    #[test]
    fn debug_redacts_the_admin_credential() {
        // A derived Debug would put the verbatim admin token in any embedder's log the first time
        // someone formats the request. Same rule `intercept_control` applies to CA key material.
        let req = AuthzRequest {
            credential: Some("super-secret-token"),
            action: actions::IMPOSTER_WRITE,
            port: Some(4545),
            space: None,
            scope: None,
            params: &[],
        };
        let rendered = format!("{req:?}");
        assert!(
            !rendered.contains("super-secret-token"),
            "the credential leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "got: {rendered}");
        // Absence must stay distinguishable from redaction, or a missing credential reads as one
        // that was merely hidden.
        let anon = AuthzRequest {
            credential: None,
            ..req
        };
        assert!(format!("{anon:?}").contains("credential: None"));
    }

    #[test]
    fn action_strings_are_distinct() {
        // A duplicated constant would silently collapse two route classes into one permission.
        let all = [
            actions::SYSTEM_READ,
            actions::SYSTEM_WRITE,
            actions::IMPOSTER_READ,
            actions::IMPOSTER_WRITE,
            actions::IMPOSTER_DELETE,
            actions::IMPOSTER_VERIFY,
            actions::EVENTS_READ,
            actions::INTERCEPT_READ,
            actions::INTERCEPT_WRITE,
        ];
        let unique: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "action constants must be distinct");
    }
}
