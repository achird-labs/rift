#[test]
fn test_timeout_resolution() {
    use rift_core::scripting::resolve_script_timeout_ms;
    
    // Test with empty config (no _rift section)
    let config = serde_json::from_value(serde_json::json!({
        "port": 19750, "protocol": "http", "stubs": [
            { "responses": [{ "inject": "function (config) { throw new Error('boom-inject'); }" }] }
        ]
    })).expect("config");
    
    let timeout = resolve_script_timeout_ms(&config);
    println!("Resolved timeout: {}ms", timeout);
    assert_eq!(timeout, 5000, "default timeout should be 5000ms");
}
