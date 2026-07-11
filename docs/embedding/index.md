---
layout: default
title: Embedding & SPI
nav_order: 10
has_children: true
permalink: /embedding/
---

# Embedding & Extension (SPI)

Rift is not only a standalone binary — it is a **library** you can embed in a Rust process, drive
over a stable **C-ABI** from any language, and extend through **service-provider-interface (SPI)**
traits. This section documents that surface: how to run the server in-process, how to bind its admin
and metrics planes to your own addresses, how to plug in custom storage backends, and how to call it
through the FFI.

Everything here reflects the code that ships today. If a signature in these pages ever disagrees with
the source, the source wins — please file an issue.

---

## When to use which entry point

| You want to… | Use | Page |
|:-------------|:----|:-----|
| Run the full Rift server (imposters + admin API + metrics) inside a Rust host | `ServerBuilder` | [Embeddable Server]({{ site.baseurl }}/embedding/server/) |
| Bind the admin or metrics plane to a chosen (or ephemeral `:0`) address and learn the bound port | `AdminApiServer::bind` / `bind_metrics_server` | [Embeddable Server]({{ site.baseurl }}/embedding/server/) |
| Replace a storage backend (flow-state, request journal, proxy recording, response sequencing) | SPI traits on `ImposterManager` | [Extension Points (SPI)]({{ site.baseurl }}/embedding/spi/) |
| Observe reconciliation events or decorate responses | `ImposterEventListener` / `ResponseDecorator` | [Extension Points (SPI)]({{ site.baseurl }}/embedding/spi/) |
| Drive Rift from a non-Rust host (JVM, Node, Go, …) | The C-ABI (`rift-ffi`) | [FFI (C-ABI)]({{ site.baseurl }}/embedding/ffi/) |

---

## Crate map

| Crate | Role | Exposes |
|:------|:-----|:--------|
| `rift-mock-core` | The engine library — no CLI, no HTTP server wiring. | `ImposterManager` and the SPI traits (`FlowStoreProvider`, `ResponseSequencer`, `RequestJournal`, `ProxyRecordingStore`, `ImposterEventListener`, `ResponseDecorator`), behaviors, predicates, scripting. |
| `rift-http-proxy` | The server crate — builds the `rift` binary and hosts the admin/metrics HTTP layer. | `ServerBuilder`, `RunningServer`, `AdminApiServer`, `bind_metrics_server`, the single-port gateway, `install_default_crypto_provider()`. |
| `rift-ffi` | The C-ABI shared library (`cdylib`) plus an `rlib` for in-crate tests. | The `extern "C"` functions (`rift_start`, `rift_serve_admin`, …) and the cbindgen header. |
| `packages/rift-node` | A Node.js consumer of the above (prebuilt binaries). | Not a Rust SPI crate — an example downstream consumer. |

### Cargo features

The engines and allocator are feature-gated. Relevant features:

| Feature | Default (`rift-http-proxy`) | Default (`rift-ffi`) | Effect |
|:--------|:----------------------------|:---------------------|:-------|
| `redis-backend` | on | on | Redis flow-store backend |
| `javascript` | on | on | JavaScript scripting engine (Boa) |
| `mimalloc` | on | **never forwarded** | mimalloc global allocator (a `cdylib` must not impose an allocator on its host, so `rift-ffi` deliberately never enables it) |

---

## A minimal embedding

```rust
use rift_http_proxy::{ServerBuilder, Cli, install_default_crypto_provider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Required before serving any HTTPS imposter — installs the rustls (ring) provider once.
    install_default_crypto_provider();

    // Run the standard server exactly as the `rift` binary would, from parsed CLI options.
    ServerBuilder::from_cli(Cli::parse()).run().await
}
```

See [Embeddable Server]({{ site.baseurl }}/embedding/server/) for injecting a custom
`ImposterManager`, binding to ephemeral ports, and graceful shutdown.
