# rift-mock-core

The engine behind [Rift](https://github.com/achird-labs/rift), a Mountebank-compatible mock server,
as a library: imposter lifecycle and listeners (HTTP/1.1, HTTP/2, TLS and mutual TLS), stub
matching, responses and behaviors, proxy record/replay, fault injection, Rhai and JavaScript
scripting, response templates and flow state — with no CLI and no admin HTTP server.

The `rift` binary (`rift-http-proxy`) and the C ABI (`rift-ffi`) are thin consumers of this crate.
Depend on it directly when you want imposters inside a Rust process without the admin API:

```rust,ignore
use rift_mock_core::imposter::{ImposterConfig, ImposterManager};

let manager = ImposterManager::new();
let config: ImposterConfig = serde_json::from_str(r#"{
    "port": 0,
    "protocol": "http",
    "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "hello" } }] }]
}"#)?;
let port = manager.create_imposter(config).await?; // port 0 is auto-assigned
```

`ImposterManager` is also the extension point. Its `with_*` builders take the SPI traits — flow
store, request journal, proxy-recording store, response sequencer, event listener, response
decorator, no-match interceptor, exchange inspector — and the built-in behaviour stays in place for
any you do not install. The Redis flow store lives in the separate `rift-store-redis` crate and
plugs in through `with_flow_store_backends`, so this crate never depends on `redis`.

## Features

| Feature | Default | Effect |
|:--------|:--------|:-------|
| `javascript` | on | JavaScript (Boa) for `inject`, `decorate` and `_rift.script` |
| `quamina-matching` | on | Quamina-backed body-field candidate pruning; matching results are identical without it |

## Documentation

- [Embedding & SPI](https://achird-labs.github.io/rift/embedding/) — the crate map, the embeddable
  server and every SPI trait
- [Concepts](https://achird-labs.github.io/rift/concepts/) — the request lifecycle and the Rift model

## License

Apache-2.0
