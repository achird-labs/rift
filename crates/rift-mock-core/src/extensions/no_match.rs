//! The no-match interceptor seam (issue #819): a last-chance hook consulted when an imposter
//! genuinely has no stub matching an incoming request, before any of the built-in no-match
//! fallthrough kicks in.

use std::future::Future;

/// What the interceptor saw. Borrowed from the in-flight request; nothing is cloned on the hot
/// path (the hook is only ever constructed after matching has already failed).
#[derive(Debug, Clone, Copy)]
pub struct NoMatchContext<'a> {
    /// The imposter's bound port.
    pub port: u16,
    pub method: &'a str,
    pub path: &'a str,
}

/// The interceptor's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoMatchDirective {
    /// Fall through to defaultForward / defaultResponse / empty-200, unchanged.
    Proceed,
    /// Re-run stub matching exactly once; a hit is served as a normal match, a second miss falls
    /// through as `Proceed` would have.
    RetryMatch,
}

/// A last-chance hook for a request that matched no stub.
///
/// Design rulings (issue #819):
///
/// - Consulted ONLY on a genuine no-match, BEFORE the defaultForward/defaultResponse/empty-200
///   fallthrough. Never for matched requests, disabled imposters, matcher errors, or the debug
///   path — zero cost on the hot path.
/// - It fires even when `defaultForward`/`defaultResponse` IS configured, deliberately: under
///   replication lag the right stub may be momentarily missing and the request would otherwise
///   be misdirected upstream. Rescue outranks forwarding.
/// - Implementations must be bounded — the request is parked while this future runs.
/// - At most ONE retry per request; a second miss falls through exactly as `Proceed` would.
/// - Annotations (`extensions::decorate::annotate`) are visible wherever a `ResponseDecorator`
///   is wired (the serve loop); on the `/__rift/` gateway they are inert, since that path has
///   neither an annotation scope nor a decorator.
/// - Out of scope: a request to a port with NO imposter never reaches any imposter handler (no
///   listener, or the gateway 404s first), so there is nothing to hang a hook on. Embedders must
///   cover that window with their own readiness gating.
/// # `RetryMatch` re-runs the whole matching pass
///
/// "Indistinguishable from a first-try match" describes everything *downstream* of the match —
/// the scenario FSM, the single cycler advance, and response dispatch all behave identically. It
/// does **not** describe the match itself: a retry re-evaluates predicates, so a predicate
/// `inject` script executes a second time and its persistent `state` mutations and `logger` output
/// are committed twice. Worst-case matching wall-clock for a rescued request is also two
/// `scriptEngine.timeoutMs` budgets plus however long this hook parks the request.
pub trait NoMatchInterceptor: Send + Sync {
    fn on_no_match<'a>(
        &'a self,
        ctx: NoMatchContext<'a>,
    ) -> std::pin::Pin<Box<dyn Future<Output = NoMatchDirective> + Send + 'a>>;
}
