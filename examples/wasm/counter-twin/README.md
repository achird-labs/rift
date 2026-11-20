# counter-twin

A `rift:mock` component for `_rift.script` with `engine: "wasm"`. Each request increments `count`
in the imposter's flow store and answers `{"count":N}`. With `knobs.limit` set, a count above the
limit answers `409`. Each request prints `count is now N` to stdout, which Rift logs as script
output (visible in the `x-rift-script-trace` header under `RIFT_DEBUG=1`).

Build it, then load the imposter next to it:

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
rift --allowInjection --configfile imposter.json
curl localhost:4561/count   # {"count":1}
curl localhost:4561/count   # {"count":2}
curl localhost:4561/count   # 409 {"error":"above limit","count":3,"limit":2}
```

The `file` path in `imposter.json` resolves against the config file's directory. Through the admin
API it resolves against `--scripts-dir`.

The WIT it builds against is `crates/rift-mock-core/wit/rift-mock.wit`; see
[WebAssembly scripting](../../../docs/features/wasm-scripting.md).
