// Fixture for `rift script run` / `rift script check` (issue #360).
//
// Fails the first two attempts (503) then passes — a common chaos-testing shape. `attempts` is
// read from flow state (`ctx.state`), seeded here by `--state attempts=<n>` rather than
// incremented by the script itself, so a single `script run` invocation is deterministic:
//
//   rift script run crates/rift-http-proxy/tests/fixtures/fail-twice.rhai --state attempts=2
//   -> pass()            (>= 2 prior attempts already recorded: let the request through)
//
//   rift script run crates/rift-http-proxy/tests/fixtures/fail-twice.rhai --state attempts=1
//   -> http(503) ...     (< 2 prior attempts: still failing)
fn respond(ctx) {
    let attempts = ctx.state.get_or("attempts", 0);
    ctx.logger.info(`attempts so far: ${attempts}`);
    if attempts < 2 {
        http(503, `Service unavailable (attempt ${attempts})`).header("Retry-After", "1")
    } else {
        pass()
    }
}
