---
layout: default
title: Extension Points (SPI)
parent: Embedding & SPI
nav_order: 2
---

# Extension Points (SPI)

Rift's storage and observation seams are **traits** in `rift-core`. An embedding host implements a
trait and injects it through a builder method on `ImposterManager`; if you don't, Rift uses its
built-in in-memory (or Redis, where applicable) implementation. The built-ins never fail — a custom
backend may, and Rift surfaces that failure explicitly (see [Backend errors](#backend-errors-and-annotations)).

All injection is via `ImposterManager` builder methods:

```rust
use std::sync::Arc;
use rift_core::imposter::ImposterManager;

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

## Backend errors and annotations

A custom backend signals unavailability by attaching `BackendUnavailable` to a failed operation's
error (backends wrap with `.context(...)`, and the marker survives the chain):

```rust
pub struct BackendUnavailable {
    pub feature: &'static str,
    pub detail: String,
}
```

`backend_error_response(&anyhow::Error)` maps such an error to a structured
`503 {"error":"backendUnavailable", ...}`; any other error maps to `500`. This is how a down remote
store becomes a clean 503 to the API caller rather than a silent fallback.

Per-request operational metadata travels through a tokio task-local annotation scope:
`annotate(key: &'static str, value: String)` records a `(key, value)` that a `ResponseDecorator` later
reads. This is the same mechanism behind the script/behavior error headers — e.g. a script that hits a
down flow-store backend records an annotation, and a v2 `ctx.state` call against that backend is
**fail-loud**: it raises a script error that surfaces to the response rather than silently returning a
default (see [Scripting → `ctx.state` and `ctx.store`]({{ site.baseurl }}/features/scripting/#ctxstate-and-ctxstore)).
