//! `rift:mock` components for the wasm engine's tests (issue #1751), built from hand-written core
//! modules and the real `wit/rift-mock.wit`, so no wasm toolchain is needed to run them.
//!
//! Each core module follows the canonical ABI: `init(knobs)` takes the knobs string as `(ptr, len)`
//! and returns a pointer to its `result<_, string>`; `respond(req)` takes a pointer to the request
//! (its flattened form has more than 16 values) and returns a pointer to its
//! `result<decision, string>`. The layout of that result, which the fixtures write as data:
//!
//! | offset | field |
//! |---|---|
//! | 64 | result discriminant: 0 ok, 1 err |
//! | 72 | decision discriminant: 0 pass, 1 delay, 2 reset, 3 respond (err: string ptr) |
//! | 76 | err: string len |
//! | 80 | delay: u64 ms; respond: status u16 |
//! | 84, 88 | respond: headers ptr, len |
//! | 92, 96 | respond: body ptr, len |

#![allow(dead_code)]

use base64::Engine as _;

const RIFT_MOCK_WIT: &str = include_str!("../wit/rift-mock.wit");

/// The extra packages fixtures import: one function each, enough for wasmtime to name the import.
const EXTRA_WIT: &[(&str, &str)] = &[
    (
        "wasi-sockets.wit",
        "package wasi:sockets@0.2.6; interface tcp { stub: func(); }",
    ),
    (
        "wasi-http.wit",
        "package wasi:http@0.2.6; interface outgoing-handler { stub: func(); }",
    ),
    (
        "wasi-cli.wit",
        "package wasi:cli@0.2.6; interface environment { get-environment: func() -> list<tuple<string, string>>; }",
    ),
    (
        "wasi-clocks.wit",
        "package wasi:clocks@0.2.6;
         interface wall-clock {
           record datetime { seconds: u64, nanoseconds: u32 }
           now: func() -> datetime;
         }
         interface monotonic-clock {
           type instant = u64;
           now: func() -> instant;
         }",
    ),
    (
        "wasi-random.wit",
        "package wasi:random@0.2.6; interface random { get-random-u64: func() -> u64; }",
    ),
    (
        "fixture.wit",
        "package test:fixture;
         world mock { include rift:mock/mock@0.1.0; }
         world sockets { include rift:mock/mock@0.1.0; import wasi:sockets/tcp@0.2.6; }
         world http { include rift:mock/mock@0.1.0; import wasi:http/outgoing-handler@0.2.6; }
         world cli { include rift:mock/mock@0.1.0; import wasi:cli/environment@0.2.6; }
         world no-handler { import rift:mock/state@0.1.0; }
         world clocks { include rift:mock/mock@0.1.0; import wasi:clocks/wall-clock@0.2.6; import wasi:clocks/monotonic-clock@0.2.6; }
         world random { include rift:mock/mock@0.1.0; import wasi:random/random@0.2.6; }",
    ),
];

/// A component from a core module (`wat`) and one of the fixture worlds.
pub fn component(world: &str, core_wat: &str) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    resolve
        .push_str("rift-mock.wit", RIFT_MOCK_WIT)
        .expect("rift-mock.wit parses");
    let mut fixture = None;
    for (name, text) in EXTRA_WIT {
        fixture = Some(resolve.push_str(name, text).expect("fixture wit parses"));
    }
    let world = resolve
        .select_world(&[fixture.expect("fixture package")], Some(world))
        .expect("fixture world");
    let mut module = wat::parse_str(core_wat).expect("core wat parses");
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .expect("embed metadata");
    wit_component::ComponentEncoder::default()
        .module(&module)
        .expect("module")
        .validate(true)
        .encode()
        .expect("encode component")
}

/// `bytes` as `_rift.script.code` carries them.
pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A `rift:mock` core module. `respond_body` is the body of `respond`, which leaves the pointer to
/// its result on the stack; `extra` is spliced into the module (imports, data, globals).
pub fn core(imports: &str, extra: &str, respond_body: &str) -> String {
    format!(
        r#"(module
  {imports}
  (memory (export "memory") 1)
  (global $bump (mut i32) (i32.const 4096))
  (global $knobs_ptr (mut i32) (i32.const 0))
  (global $knobs_len (mut i32) (i32.const 0))
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
    (local $p i32)
    (local.set $p
      (i32.and
        (i32.add (global.get $bump) (i32.sub (local.get 2) (i32.const 1)))
        (i32.sub (i32.const 0) (local.get 2))))
    (global.set $bump (i32.add (local.get $p) (local.get 3)))
    (local.get $p))
  (func (export "rift:mock/handler@0.1.0#init") (param i32 i32) (result i32)
    (global.set $knobs_ptr (local.get 0))
    (global.set $knobs_len (local.get 1))
    (i32.const 0))
  (func (export "rift:mock/handler@0.1.0#respond") (param i32) (result i32)
    {respond_body})
  {extra}
)"#
    )
}

/// `respond` answers `200 ok` with header `x-twin: yes`.
pub fn respond_ok() -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(data (i32.const 64) "\00")
               (data (i32.const 72) "\03")
               (data (i32.const 80) "\c8\00")
               (data (i32.const 84) "\2c\01\00\00\01\00\00\00")
               (data (i32.const 92) "\00\01\00\00\02\00\00\00")
               (data (i32.const 256) "ok")
               (data (i32.const 300) "\90\01\00\00\06\00\00\00\9a\01\00\00\03\00\00\00")
               (data (i32.const 400) "x-twin")
               (data (i32.const 410) "yes")"#,
            "(i32.const 64)",
        ),
    )
}

/// `respond` returns `decision` with no payload: `0` pass, `2` reset.
pub fn respond_bare(decision: u8) -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            &format!(r#"(data (i32.const 64) "\00") (data (i32.const 72) "\{decision:02x}")"#),
            "(i32.const 64)",
        ),
    )
}

/// `respond` returns `delay(250)`.
pub fn respond_delay_250() -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\01") (data (i32.const 80) "\fa\00\00\00\00\00\00\00")"#,
            "(i32.const 64)",
        ),
    )
}

/// `respond` returns `err("guest says no")`.
pub fn respond_guest_error() -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(data (i32.const 64) "\01") (data (i32.const 72) "\00\01\00\00\0d\00\00\00") (data (i32.const 256) "guest says no")"#,
            "(i32.const 64)",
        ),
    )
}

/// `respond` answers 200 with the knobs `init` received as its body.
pub fn respond_knobs() -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\03") (data (i32.const 80) "\c8\00")"#,
            "(i32.store (i32.const 92) (global.get $knobs_ptr))
             (i32.store (i32.const 96) (global.get $knobs_len))
             (i32.const 64)",
        ),
    )
}

/// `respond` answers 200 with a body that is not UTF-8.
pub fn respond_binary_body() -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\03") (data (i32.const 80) "\c8\00")
               (data (i32.const 92) "\00\01\00\00\02\00\00\00") (data (i32.const 256) "\ff\fe")"#,
            "(i32.const 64)",
        ),
    )
}

/// `respond` never returns.
pub fn spin() -> Vec<u8> {
    component("mock", &core("", "", "(loop $l (br $l)) (unreachable)"))
}

/// `respond` grows its memory by 1 GiB, then passes.
pub fn grow_1gib() -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\00")"#,
            "(drop (memory.grow (i32.const 16384))) (i32.const 64)",
        ),
    )
}

/// `respond` calls `state.incr-by("count", 1, none)` and passes.
pub fn incr_count() -> Vec<u8> {
    component(
        "mock",
        &core(
            r#"(import "rift:mock/state@0.1.0" "incr-by" (func $incr (param i32 i32 i64 i32 i32 i32 i32)))"#,
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\00") (data (i32.const 256) "count")"#,
            "(call $incr (i32.const 256) (i32.const 5) (i64.const 1) (i32.const 0) (i32.const 0) (i32.const 0) (i32.const 512))
             (i32.const 64)",
        ),
    )
}

/// A component that also imports `import_world`'s extra interface (`sockets`, `http` or `cli`).
pub fn importing(import_world: &str) -> Vec<u8> {
    let import = match import_world {
        "sockets" => r#"(import "wasi:sockets/tcp@0.2.6" "stub" (func $x))"#,
        "http" => r#"(import "wasi:http/outgoing-handler@0.2.6" "stub" (func $x))"#,
        "cli" => r#"(import "wasi:cli/environment@0.2.6" "get-environment" (func $x (param i32)))"#,
        other => panic!("no fixture world {other}"),
    };
    let call = if import_world == "cli" {
        "(call $x (i32.const 512))"
    } else {
        "(call $x)"
    };
    component(
        import_world,
        &core(
            import,
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\00")"#,
            &format!("{call} (i32.const 64)"),
        ),
    )
}

/// A component that exports no `rift:mock/handler`.
pub fn no_handler() -> Vec<u8> {
    component(
        "no-handler",
        r#"(module (memory (export "memory") 1)
             (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 4096)))"#,
    )
}

/// A core module, not a component.
pub fn core_module() -> Vec<u8> {
    wat::parse_str("(module)").expect("module")
}

/// `respond` answers 200 with the request's body as its body. The request record's `body` list is
/// at offset 40 of the record `respond` receives a pointer to.
pub fn respond_echo_body() -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\03") (data (i32.const 80) "\c8\00")"#,
            "(i32.store (i32.const 92) (i32.load offset=40 (local.get 0)))
             (i32.store (i32.const 96) (i32.load offset=44 (local.get 0)))
             (i32.const 64)",
        ),
    )
}

/// `respond` answers with status `200 + n`, where `n` is the number of environment variables
/// `wasi:cli/environment.get-environment` returns.
pub fn respond_env_count() -> Vec<u8> {
    component(
        "cli",
        &core(
            r#"(import "wasi:cli/environment@0.2.6" "get-environment" (func $env (param i32)))"#,
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\03")"#,
            "(call $env (i32.const 512))
             (i32.store16 (i32.const 80) (i32.add (i32.const 200) (i32.load (i32.const 516))))
             (i32.const 64)",
        ),
    )
}

/// `init` returns `err("bad knobs")`.
pub fn init_error() -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\00")
               (data (i32.const 1024) "\01\00\00\00\00\05\00\00\09\00\00\00") (data (i32.const 1280) "bad knobs")"#,
            "(i32.const 64)",
        )
        // `init` points at its own result instead of the zeroed `ok` at 0.
        .replace(
            "(global.set $knobs_len (local.get 1))\n    (i32.const 0))",
            "(global.set $knobs_len (local.get 1))\n    (i32.const 1024))",
        ),
    )
}

/// `respond` executes `unreachable`.
pub fn unreachable_trap() -> Vec<u8> {
    component("mock", &core("", "", "(unreachable)"))
}

/// A component whose core module has a second memory; `respond` grows both by `pages` (64 KiB
/// each) and passes. Exercises the call-wide memory cap.
pub fn two_memories(pages: u32) -> Vec<u8> {
    component(
        "mock",
        &core(
            "",
            r#"(memory $second 0)
               (data (i32.const 64) "\00") (data (i32.const 72) "\00")"#,
            &format!(
                "(drop (memory.grow (i32.const {pages})))
                 (drop (memory.grow $second (i32.const {pages})))
                 (i32.const 64)"
            ),
        ),
    )
}

/// `respond` reads `wasi:clocks/wall-clock.now` and answers `delay(<its seconds>)`, so a test reads
/// the guest's wall time from the decision (issue #1754).
pub fn wall_clock_seconds() -> Vec<u8> {
    component(
        "clocks",
        &core(
            r#"(import "wasi:clocks/wall-clock@0.2.6" "now" (func $now (param i32)))"#,
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\01")"#,
            "(call $now (i32.const 512))
             (i64.store (i32.const 80) (i64.load (i32.const 512)))
             (i32.const 64)",
        ),
    )
}

/// `respond` reads `wasi:clocks/monotonic-clock.now` and answers `delay(<its nanoseconds>)`.
pub fn monotonic_nanos() -> Vec<u8> {
    component(
        "clocks",
        &core(
            r#"(import "wasi:clocks/monotonic-clock@0.2.6" "now" (func $now (result i64)))"#,
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\01")"#,
            "(i64.store (i32.const 80) (call $now))
             (i32.const 64)",
        ),
    )
}

/// `respond` draws `wasi:random/random.get-random-u64` and answers `200` with it as an 8-byte body,
/// each byte masked to ASCII so the body is UTF-8 (issue #1755).
pub fn random_body() -> Vec<u8> {
    component(
        "random",
        &core(
            r#"(import "wasi:random/random@0.2.6" "get-random-u64" (func $r (result i64)))"#,
            r#"(data (i32.const 64) "\00")
               (data (i32.const 72) "\03")
               (data (i32.const 80) "\c8\00")
               (data (i32.const 84) "\00\00\00\00\00\00\00\00")
               (data (i32.const 92) "\00\01\00\00\08\00\00\00")"#,
            "(i64.store (i32.const 256) (i64.and (call $r) (i64.const 0x7F7F7F7F7F7F7F7F)))
             (i32.const 64)",
        ),
    )
}

/// `respond` draws `wasi:random/random.get-random-u64` and answers `delay(<it>)`, so a test reads the
/// guest's draw from the decision (issue #1755).
pub fn random_u64() -> Vec<u8> {
    component(
        "random",
        &core(
            r#"(import "wasi:random/random@0.2.6" "get-random-u64" (func $r (result i64)))"#,
            r#"(data (i32.const 64) "\00") (data (i32.const 72) "\01")"#,
            "(i64.store (i32.const 80) (call $r))
             (i32.const 64)",
        ),
    )
}
