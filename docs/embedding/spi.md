---
layout: default
title: Extension Points (SPI)
parent: Embedding & SPI
nav_order: 2
---

# Extension Points (SPI)

Rift's storage and observation seams are **traits** in `rift-mock-core`. An embedding host implements a
trait and injects it through a builder method on `ImposterManager`; if you don't, Rift uses its
built-in in-memory (or Redis, where applicable) implementation. The built-ins never fail — a custom
backend may, and Rift surfaces that failure explicitly (see [Backend errors](#backend-errors-and-annotations)).

All injection is via `ImposterManager` builder methods:

```rust
use std::sync::Arc;
use rift_mock_core::imposter::ImposterManager;

let manager = ImposterManager::new()
    .with_flow_store_provider(Arc::new(MyFlowStores))
    .with_sequencer(Arc::new(MySequencer))
    .with_request_journal(Arc::new(MyJournal))
    .with_proxy_store(Arc::new(MyProxyStore))
    .with_event_listener(Arc::new(MyListener))
    .with_response_decorator(Arc::new(MyDecorator));
```

Then pass `Arc::new(manager)` to `ServerBuilder::manager(...)` (see
[Embeddable Server]({{ site.baseurl }}/embedding/server/)).

---

## `FlowStoreProvider` — custom flow-state backend

Provide a [flow-state]({{ site.baseurl }}/features/flow-state/) store per imposter, or return `None`
to defer to the built-ins (in-memory / Redis).

```rust
pub trait FlowStoreProvider: Send + Sync {
    /// Return a store for this imposter, or `None` to defer to the built-ins.
    fn provide(&self, config: &ImposterConfig) -> Option<Arc<dyn FlowStore>>;
}
```

Inject with `.with_flow_store_provider(Arc<dyn FlowStoreProvider>)`.

A provider **overrides** the imposter's own `_rift.flowState` selection, and it cannot report a
failure — `provide` returns `Option`, so declining falls through to the built-ins. When you want a
store the *config* selects by name, and misconfiguration to fail loudly, use
`FlowStoreBackendFactory` below instead.

---

## `FlowStoreBackendFactory` — a named flow-state backend

Adds a `_rift.flowState.backend` name the config can select, with an error channel. This is how the
Redis backend ships: it lives in the separate `rift-store-redis` crate, so `rift-mock-core` itself
carries no `redis`/`r2d2` dependency under any feature combination.

```rust
pub trait FlowStoreBackendFactory: Send + Sync {
    /// The `_rift.flowState.backend` string this factory serves, e.g. "redis".
    fn name(&self) -> &'static str;

    /// Build a store for this imposter's flowState block. An `Err` fails imposter creation.
    fn build(&self, config: &RiftFlowStateConfig) -> anyhow::Result<Arc<dyn FlowStore>>;
}
```

Register with `.with_flow_store_backends(FlowStoreBackends)`:

```rust
use rift_mock_core::extensions::flow_state::FlowStoreBackends;

let backends = FlowStoreBackends::new().with(Arc::new(MyBackend));
let manager = ImposterManager::new().with_flow_store_backends(backends);
```

The `rift` binary and the C-ABI register their shipped backends automatically — with the default
`redis-backend` feature that means `"redis"`, so `_rift.flowState.backend: "redis"` works out of the
box. `rift_http_proxy::default_flow_store_backends()` returns that set if you are assembling a
manager yourself and want the same vocabulary.

Choosing between the two seams:

| | `FlowStoreProvider` | `FlowStoreBackendFactory` |
|---|---|---|
| Selected by | the embedder, for every imposter | the imposter's `flowState.backend` name |
| Precedence | overrides `_rift.flowState` | only consulted when the config names it |
| On failure | can only decline (`None`) → falls through | returns `Err` → imposter creation fails with `400` |

A backend name that nothing registered is a config error, never a silent downgrade to a no-op store:
creation fails with an error listing the names this build does serve.

## `ResponseSequencer` — custom response cycling

Owns the per-stub cursor that drives multiple-response cycling and `repeat` (see
[Behaviors → repeat]({{ site.baseurl }}/mountebank/behaviors/#repeat)).

```rust
pub trait ResponseSequencer: Send + Sync {
    /// Atomically advance and return the response index, honoring per-response repeats.
    fn next(&self, key: SequenceKey<'_>, response_count: usize, repeats: &[u32]) -> Result<usize>;
    /// Return the upcoming response index without advancing.
    fn peek(&self, key: SequenceKey<'_>, response_count: usize, repeats: &[u32]) -> Result<usize>;
    /// Reset cursors: one stub's (`Some(stub_key)`) or every cursor on the port (`None`).
    /// Also the GC hook — called on stub delete, bulk stub replace, and imposter teardown.
    fn reset_scope(&self, port: u16, stub_key: Option<&str>);
}
```

Inject with `.with_sequencer(Arc<dyn ResponseSequencer>)`.

## `RequestJournal` — custom recorded-requests store

Backs `recordRequests`, `numberOfRequests`, and the `savedRequests` admin surface.

```rust
pub trait RequestJournal: Send + Sync {
    /// Called for EVERY request (even when body recording is off) — backs `numberOfRequests`.
    fn note_request(&self, port: u16);
    /// `flow_id` is the request's resolved flow (per the imposter's `flowIdSource`).
    fn record(&self, port: u16, flow_id: &str, req: RecordedRequest);
    fn read(&self, port: u16) -> JournalRead;
    /// Clears entries AND resets the request count. Fallible — a remote store may fail.
    fn clear(&self, port: u16) -> anyhow::Result<()>;
    fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool);
    /// Clear just one flow's entries. Fallible.
    fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()>;
    fn count(&self, port: u16) -> u64;
}
```

Note that `clear` and `clear_flow` are **fallible** (`anyhow::Result<()>`): clearing is a correctness
operation whose postcondition ("the data is gone") a remote backend can fail to guarantee, so the
failure propagates rather than being swallowed. Inject with `.with_request_journal(Arc<dyn RequestJournal>)`.

## `ProxyRecordingStore` — custom proxy-recording store

Backs [proxy record/replay]({{ site.baseurl }}/mountebank/proxy/): claims the right to record a
response once per request signature, then stores and looks up recordings.

```rust
pub trait ProxyRecordingStore: Send + Sync {
    /// First caller per `(port, signature)` wins the right to record once.
    /// `Err` = backend unavailable (built-ins never fail).
    fn try_claim(&self, port: u16, sig: &RequestSignature) -> Result<ClaimOutcome>;
    /// Release a claim after a failed upstream call so the signature is retryable.
    fn release_claim(&self, port: u16, sig: &RequestSignature, token: ClaimToken);
    fn record(&self, /* port, sig, response, token */) -> Result<()>;
    fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse>;
    fn clear(&self, port: u16);
}
```

Its typed error is `ProxyStoreError` (`ProxyStoreError::Unavailable(String)`). Inject with
`.with_proxy_store(Arc<dyn ProxyRecordingStore>)`.

## `ImposterEventListener` — observe reconciliation

Get a callback whenever the imposter set changes (startup load, `POST /admin/reload`, admin CRUD).
See [Hot Reload]({{ site.baseurl }}/features/hot-reload/) for how the incremental diff produces these.

```rust
pub enum ImposterEvent {
    Created(u16),      // port created
    Replaced(u16),     // port replaced (imposter-level change)
    StubsChanged(u16), // in-place stub patch
    Deleted(u16),      // port deleted
    AllDeleted,        // every imposter removed
}

pub trait ImposterEventListener: Send + Sync {
    fn on_event(&self, event: &ImposterEvent);
}
```

`on_event` is called **synchronously on the mutating path** — keep implementations fast and
non-blocking. Inject with `.with_event_listener(Arc<dyn ImposterEventListener>)`.

## `ResponseDecorator` — cross-cutting response headers

A hook to add operational headers to outgoing responses based on the request phase and per-request
annotations.

```rust
pub trait ResponseDecorator: Send + Sync {
    fn decorate(
        &self,
        phase: ResponsePhase,
        req_port: Option<u16>,
        annotations: &[(&'static str, String)],
        headers: &mut HeaderMap,
    );
}
```

Inject with `.with_response_decorator(Arc<dyn ResponseDecorator>)`.

---

## `NoMatchInterceptor` — rescue the no-match path

Consulted when a request matched **no stub**, before the `defaultForward` / `defaultResponse` /
empty-`200` fallthrough (issue #819). Its purpose is a safety net on the data plane: an embedder
whose replicated config is momentarily behind can wait a bounded interval and retry the match once,
paying nothing on requests that already matched.

```rust
use rift_mock_core::extensions::no_match::{
    NoMatchContext, NoMatchDirective, NoMatchInterceptor,
};

struct WaitForCatchUp;

impl NoMatchInterceptor for WaitForCatchUp {
    fn on_no_match<'a>(
        &'a self,
        ctx: NoMatchContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = NoMatchDirective> + Send + 'a>> {
        Box::pin(async move {
            if caught_up_within(ctx.port, Duration::from_millis(500)).await {
                NoMatchDirective::RetryMatch
            } else {
                NoMatchDirective::Proceed
            }
        })
    }
}

let manager = ImposterManager::new()
    .with_no_match_interceptor(Arc::new(WaitForCatchUp));
```

Contract:

- **Only on a genuine no-match.** Never for matched requests, disabled imposters, matcher errors, or
  the debug path — the hot path pays nothing when nothing missed.
- **It fires even when a default IS configured**, deliberately. Under replication lag the right stub
  may be momentarily missing, and forwarding upstream would misdirect the request; rescue outranks
  forwarding. `Proceed` then falls through exactly as today.
- **At most one retry per request.** A rescued hit is served as a normal match — indistinguishable
  downstream — and a second miss falls through exactly as `Proceed` would. "Downstream" is precise:
  the retry re-evaluates predicates, so a predicate `inject` script runs a **second** time and its
  `state` mutations and `logger` output are committed twice; a rescued request can also spend two
  `scriptEngine.timeoutMs` budgets in matching.
- **Implementations must be bounded**: the request is parked while the future runs.
- **Annotations** (`extensions::decorate::annotate`) are visible wherever a `ResponseDecorator` is
  wired — the serve loop. On the `/__rift/` gateway they are inert, since that path has neither an
  annotation scope nor a decorator.
- **Unbound ports are out of scope.** A request to a port with no imposter never reaches an imposter
  handler (no listener, or the gateway `404`s first), so there is nothing to hang a hook on. Cover
  that window with your own readiness gating.

Registering no interceptor leaves behaviour byte-identical, on both the serve loop and the gateway.

## `AdminAuthorizer` — per-request admin authorization

The built-in `--api-key` gate yields *access*, not an identity: every caller that presents the key
is equivalent. `AdminAuthorizer` lets an embedder decide per request, with the route already parsed.

```rust
use rift_mock_core::extensions::authz::{
    AdminAuthorizer, AuthzDecision, AuthzRequest, actions,
};

struct TenantAuthorizer;

impl AdminAuthorizer for TenantAuthorizer {
    fn authorize(&self, req: AuthzRequest<'_>) -> AuthzDecision {
        let principal = match req.credential.and_then(lookup_principal) {
            Some(p) => p,
            None => return AuthzDecision::Deny { reason: "unknown principal" },
        };
        match req.action {
            actions::IMPOSTER_DELETE if !principal.may_delete => {
                AuthzDecision::Deny { reason: "delete not permitted" }
            }
            _ => AuthzDecision::Allow { principal: Some(principal.name) },
        }
    }
}

let server = ServerBuilder::from_cli(cli)
    .admin_authorizer(Arc::new(TenantAuthorizer))
    .start()
    .await?;
```

**Install nothing, change nothing.** With no authorizer registered the api-key comparison decides
alone, exactly as before.

### Ordering is part of the contract

Authentication runs **first and unconditionally**; only then is the route parsed and the hook
consulted. That order is load-bearing — if authentication ran after route parsing, an
unauthenticated request to an unknown path would answer `404` instead of `401` and unknown-path
responses would become a route-existence oracle for anonymous callers.

- Missing or invalid credential → `401`, and the hook is **not** consulted.
- `Deny` on an authenticated request → `403` with the standard error envelope.
- A path matching no route → the ordinary `404`; the hook is not consulted, because nothing runs.

### Actions

`action` is a stable string rather than an enum, so an embedder can extend its own vocabulary
without waiting for an upstream release. The values upstream emits are constants in
`extensions::authz::actions`:

| Action | Routes |
|:--|:--|
| `system.read` | `GET /`, `/health`, `/config`, `/logs`, `/metrics` |
| `system.write` | `POST /admin/reload` |
| `imposter.read` | any `GET` under `/imposters`, and the per-imposter SSE alias |
| `imposter.write` | mutating `POST`/`PUT` on an imposter, its stubs, scenarios or flow state |
| `imposter.delete` | any `DELETE` under `/imposters` — **and `PUT /imposters`**, which reconciles the whole set and so removes everything not in the payload |
| `imposter.verify` | `POST /imposters/:port/verify`, which mutates nothing |
| `events.read` | `GET /events`, the cross-imposter stream |
| `intercept.read` / `intercept.write` | `/intercept` and below |

`events.read` is separate from `imposter.read` on purpose: `/events` carries recorded requests from
*every* imposter, so granting read on one port must not implicitly grant all of them.

### Targeting: `port`, `space`, `params`, `scope`

`port`, `space` and `params` come from the router's own parser, so they cannot drift from what the
handler will actually act on.

`scope` is different. It is read verbatim from the **`x-rift-scope` request header** and exists
because some routes have no target to key on — `POST /imposters` creates a port rather than naming
one. Because it is a request header, **it is caller-asserted**: any authenticated caller can set it
to any value. Cross-check it against what the credential entitles the caller to; never use it
directly as the authorization subject.

The data plane is never authorized. Gateway traffic (`/__rift/...`) skips this hook for the same
reason it skips the api key — it is app-under-test traffic, and requiring an admin identity for it
would force the application to carry the admin credential.

## Backend errors and annotations

A custom backend signals unavailability by attaching `BackendUnavailable` to a failed operation's
error (backends wrap with `.context(...)`, and the marker survives the chain):

```rust
pub struct BackendUnavailable {
    pub feature: &'static str,
    pub detail: String,
}
```

`backend_error_response(&anyhow::Error)` maps such an error to a structured `503`; any other error
maps to `500`. This is how a down remote store becomes a clean 503 to the API caller rather than a
silent fallback. The body carries the standard error envelope, with `feature` naming which backend
failed:

```json
{
  "errors": [{
    "code": "503",
    "type": "backend unavailable",
    "message": "flowState: redis connection refused",
    "feature": "flowState",
    "detail": "redis connection refused"
  }],
  "error": "backendUnavailable",
  "feature": "flowState",
  "detail": "redis connection refused"
}
```

> **Deprecated:** the **top-level** `error`/`feature`/`detail` keys are retained for backward
> compatibility and will be **removed in 0.17.0** (#801). Read `errors[0]` instead.

Per-request operational metadata travels through a tokio task-local annotation scope:
`annotate(key: &'static str, value: String)` records a `(key, value)` that a `ResponseDecorator` later
reads. This is the same mechanism behind the script/behavior error headers — e.g. a script that hits a
down flow-store backend records an annotation, and a `ctx.state` call against that backend is
**fail-loud**: it raises a script error that surfaces to the response rather than silently returning a
default (see [Scripting → `ctx.state` and `ctx.store`]({{ site.baseurl }}/features/scripting/#ctxstate-and-ctxstore)).
