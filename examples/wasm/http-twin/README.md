# http-twin

A standard `wasi:http/proxy` component for `_rift.script` with `engine: "wasm"` and
`export: "http"`. It also exports `rift:mock/handler`, so the same component answers under
`export: "rift"`: a twin that a simulation runtime drives directly and that Rift serves over HTTP.
Built with `--no-default-features`, it exports `wasi:http/incoming-handler` only.

`src/lib.rs` lists what each path answers. It reads its knobs from the `RIFT_KNOBS` environment
variable, and keeps a counter in the imposter's flow store through `rift:mock/state`.

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
rift --configfile imposter.json
curl -s localhost:4562/echo?x=1 -d payload   # "POST /echo?x=1", a newline, then the body
curl -si localhost:4562/echo | grep x-knobs  # x-knobs: {"region":"eu-west-1"}
curl -s localhost:4562/count                 # {"count":1}
curl -s localhost:4562/count                 # {"count":2}
```

The `file` path in `imposter.json` resolves against the config file's directory.

Rift's tests that run it (`crates/rift-http-proxy/tests/wasm_http_export.rs`, and the http case of
`tests/wasm_check.rs`) are `#[ignore]`d and read the built components from `target/`; CI's "Wasm
Example Component" job builds both and runs them. See
[WebAssembly scripting](../../../docs/features/wasm-scripting.md).
