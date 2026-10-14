//! Issue #1447: a thread-scoped log capture lost events when another thread reached the logging
//! callsite first while the capture was the only registered `tracing` dispatcher — `tracing-core`
//! then caches the other thread's "nobody listens" for every thread. Its own test binary, with a
//! single test, because the condition is process-wide: any earlier dispatcher hides it.

fn script_log(line: &str) {
    tracing::info!(target: "rift::script", "{line}");
}

#[test]
fn a_script_log_line_another_thread_reached_first_is_still_captured() {
    let ((), logs) = rift_mock_core::scripting::capture_script_logs(|| {
        std::thread::spawn(|| script_log("elsewhere"))
            .join()
            .expect("thread");
        script_log("here");
    });
    assert_eq!(logs, vec!["here".to_string()]);
}
