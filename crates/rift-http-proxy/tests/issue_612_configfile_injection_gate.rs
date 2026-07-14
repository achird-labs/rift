//! Issue #612: the `--allowInjection` gate must be wired into the `--configfile` startup door,
//! not just the admin API.
//!
//! Lives in its own test binary rather than `embedded_server.rs` on purpose: that binary's
//! `run_metrics_server_serves_metrics` depends on a sibling test having already recorded a `rift_`
//! metric into the process-global registry (it fails when run alone, on master too), so adding a
//! test there perturbs its scheduling and makes it flake.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};

// The unit tests in `server.rs` cover the gate's *decision*, and the door tests cover the loader
// calling it. Only a real `Cli::try_parse_from(...) -> ServerBuilder::start()` proves the CLI flag
// is threaded all the way to that door — which is the wiring #612 reported as missing. A hardcoded
// flag anywhere on that path fails here while every unit test still passes.
#[tokio::test]
async fn server_builder_gates_a_scripted_configfile_on_allow_injection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    std::fs::write(
        &path,
        r#"{"imposters":[{"port":19607,"protocol":"http","stubs":[
            {"responses":[{"inject":"function (req) { return {body: 'x'}; }"}]}]}]}"#,
    )
    .expect("write scripted config");

    let cli_with = |extra: &[&str]| {
        let mut args = vec![
            "rift",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--metrics-port",
            "0",
            "--configfile",
            path.to_str().expect("utf8 path"),
        ];
        args.extend_from_slice(extra);
        Cli::try_parse_from(args).expect("cli parse")
    };

    let err = ServerBuilder::from_cli(cli_with(&[]))
        .start()
        .await
        .err()
        .expect("a scripted configfile without --allow-injection must abort startup");
    assert!(
        err.to_string().contains("--allowInjection"),
        "the startup error must name the flag: {err}"
    );

    let running = ServerBuilder::from_cli(cli_with(&["--allow-injection"]))
        .start()
        .await
        .expect("--allow-injection must permit the same configfile");
    running.shutdown().await;
}
