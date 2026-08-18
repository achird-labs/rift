//! The exchange inspector seam (issue #966): a synchronous, per-imposter hook pair that sees a
//! request before it is matched and the response before it is written, and may replace either.
//!
//! What it is for: policy on **live** exchanges — request linting, contract validation against a
//! schema, compliance capture of request/response pairs, a chaos veto. Nothing upstream could stand
//! in that position before: [`NoMatchInterceptor`](crate::extensions::no_match::NoMatchInterceptor)
//! fires only on a genuine no-match, the
//! [`ResponseDecorator`](crate::extensions::decorate::ResponseDecorator) may only add headers, and
//! the admin authorizer is admin-time. It is also the primitive Mountebank's `matches` array
//! (`record_matches`, parsed and never consulted) needs: the response-side hook sees exactly the
//! request/response pair that array records.
//!
//! Design rulings:
//!
//! - **Inert by default.** [`Imposter`](crate::imposter::Imposter) carries an `Option`, the
//!   manager's provider defaults to `None`, and an imposter with no inspector pays one `is_none`
//!   check per phase. Nothing else changes: the existing suite is the proof.
//! - **Request side runs after journaling and before matching.** Rejecting before matching means a
//!   rejected request never advances a cycler, a scenario FSM or a match counter — the only
//!   defensible semantics for "this request was off-policy". The request *is* journaled (it
//!   arrived; hiding it would falsify `savedRequests`), and its entry carries the rejection's
//!   status and latency like any other.
//! - **Response side runs in the one funnel every path shares** — the serve loop, the `/__rift/`
//!   gateway and an embedder's in-process dispatch — after the response is built and before the
//!   decorator and CORS. It is not consulted for a response the request-side hook itself produced.
//! - **Early exits see neither hook.** A disabled imposter, a CORS preflight, a 413 and a body-read
//!   error return before there is anything to inspect.
//! - **Synchronous, deliberately.** The no-match hook is a boxed future because it parks an
//!   already-failed request to rescue it over the network; this one runs on every request of an
//!   opted-in imposter, and an async signature invites I/O onto the hot path. Everything an
//!   inspector legitimately needs (compiled validators, policy) is process-local.
//! - **Borrowed views.** Both views borrow what the handler already holds; nothing is cloned for an
//!   inspector that is not installed, and only the built response body is materialised for one that
//!   is.

use std::sync::Arc;

use bytes::Bytes;

use crate::imposter::{ImposterConfig, ResponseMode};

/// The request-side view: the in-flight request after body collection, borrowed.
#[derive(Debug, Clone, Copy)]
pub struct InspectRequest<'a> {
    /// The imposter's port.
    pub port: u16,
    pub method: &'a str,
    pub path: &'a str,
    /// The raw query string, without the `?`; empty when there was none.
    pub query: &'a str,
    /// Every value, in order — the same map matching sees.
    pub headers: &'a hyper::HeaderMap,
    /// The engine's lossless string form: text, or base64 when `mode` is
    /// [`ResponseMode::Binary`] — the same representation matching and the journal use.
    pub body: Option<&'a str>,
    pub mode: &'a ResponseMode,
}

/// The response-side view: what the imposter built, after behaviors ran, before it is written.
#[derive(Debug, Clone, Copy)]
pub struct InspectResponse<'a> {
    pub status: u16,
    pub headers: &'a hyper::HeaderMap,
    pub body: &'a [u8],
}

/// An inspector's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectVerdict {
    /// Continue unchanged — the only verdict a default build ever produces.
    Proceed,
    /// Replace the exchange's outcome with this response.
    Reject {
        status: u16,
        content_type: String,
        body: Bytes,
    },
}

/// A synchronous hook pair on an imposter's live exchanges. See the module doc for where each
/// hook runs and what a rejection means.
pub trait ExchangeInspector: Send + Sync {
    /// After body collection and journaling, before stub matching.
    fn inspect_request(&self, req: &InspectRequest<'_>) -> InspectVerdict;

    /// After the response is built, before it is written and decorated. `req` is the same view
    /// `inspect_request` saw.
    fn inspect_response(
        &self,
        req: &InspectRequest<'_>,
        resp: &InspectResponse<'_>,
    ) -> InspectVerdict;
}

/// Decides, per imposter, whether it gets an inspector — mirroring
/// [`FlowStoreProvider`](crate::extensions::flow_state::FlowStoreProvider): consulted once when the
/// manager creates the imposter, with that imposter's config, and `None` means that imposter runs
/// with no hooks at all.
pub trait ExchangeInspectorProvider: Send + Sync {
    fn provide(&self, config: &ImposterConfig) -> Option<Arc<dyn ExchangeInspector>>;
}
