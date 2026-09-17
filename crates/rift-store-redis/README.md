# rift-store-redis

The Redis flow-state backend for [Rift](https://github.com/achird-labs/rift).

Flow state is Rift's per-flow key/value store (scripts, `_rift.stateOps`, scenario state). The
built-in store is in-memory; this crate provides `RedisFlowStore`, so several Rift instances can
share that state. It lives outside `rift-mock-core` so the engine never depends on `redis` or
`r2d2`.

The `rift` binary and the C ABI already register it through their default `redis-backend` feature,
so an imposter selects it with configuration alone:

```json
{
  "_rift": {
    "flowState": {
      "backend": "redis",
      "redis": { "url": "redis://localhost:6379", "poolSize": 10, "keyPrefix": "rift:" }
    }
  }
}
```

An embedder that builds its own `ImposterManager` registers it explicitly:

```rust,ignore
use std::sync::Arc;
use rift_mock_core::extensions::flow_state::FlowStoreBackends;
use rift_mock_core::imposter::ImposterManager;
use rift_store_redis::RedisFlowStoreBackendFactory;

let manager = ImposterManager::new().with_flow_store_backends(
    FlowStoreBackends::new().with(Arc::new(RedisFlowStoreBackendFactory)),
);
```

Construction is fail-loud: a missing `redis` block, a bad URL or an unreachable server fails
imposter creation instead of silently falling back to a no-op store. Supports Redis 6.x and
7.x.

See [Flow State](https://achird-labs.github.io/rift/features/flow-state/) and
[Extension Points (SPI)](https://achird-labs.github.io/rift/embedding/spi/).

## License

Apache-2.0
