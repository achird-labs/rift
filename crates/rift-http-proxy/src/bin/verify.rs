//! Rift Stub Verifier CLI Tool
//!
//! This tool fetches imposter configurations and verifies that stubs respond
//! as expected by simulating API calls based on the predicate definitions.
//!
//! Usage:
//!   rift-verify --admin-url http://localhost:2525 [OPTIONS]
//!
//! Features:
//! - Fetches all imposters from the admin API
//! - Generates test requests based on stub predicates
//! - Verifies responses match expected values
//! - Optionally generates curl commands
//! - Provides detailed failure reports

// Allow unused fields that may be used in future versions or for debugging
#![allow(dead_code)]

use clap::Parser;
use reqwest::Client;
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;
use std::time::Duration;

#[path = "verify/dynamic.rs"]
mod dynamic;

// ANSI color codes
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Rift Stub Verifier - Test your imposters and stubs
#[derive(Parser, Debug)]
#[command(name = "rift-verify")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Rift admin API URL
    #[arg(short, long, default_value = "http://localhost:2525")]
    admin_url: String,

    /// Specific imposter port to verify (optional, verifies all if not specified)
    #[arg(short, long)]
    port: Option<u16>,

    /// Show curl commands for each test
    #[arg(short = 'c', long)]
    show_curl: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Request timeout in seconds
    #[arg(short, long, default_value = "10")]
    timeout: u64,

    /// Only run dry-run (don't make actual requests, just show what would be tested)
    #[arg(long)]
    dry_run: bool,

    /// Skip stubs with inject/proxy/script responses (can't verify dynamically generated responses)
    #[arg(long)]
    skip_dynamic: bool,

    /// Only verify status codes, ignore body and header mismatches
    /// Useful when multiple stubs have overlapping predicates or response cycling
    #[arg(long)]
    status_only: bool,

    /// Run a demo showing enhanced error output examples
    #[arg(long)]
    demo: bool,

    /// Route requests through the single-port gateway (`{admin_url}/__rift/<port>/...`, issue #212)
    /// instead of connecting to each imposter port directly.
    #[arg(long)]
    gateway: bool,

    /// Accept self-signed / invalid TLS certificates (needed for `protocol: https` imposters,
    /// issue #206, which typically present a self-signed cert).
    #[arg(long)]
    insecure: bool,

    /// Correlation value sent for correlated-isolation imposters (issue #223): when an imposter
    /// declares `flowIdSource: "header:<Name>"`, every request carries `<Name>: <space>`.
    #[arg(long, default_value = "rift-verify")]
    space: String,

    /// Opt-in: assert dynamic behaviors instead of skipping them (issue #251). Stands up an
    /// embedded mock upstream for `proxy`, runs any `_verify` sequence against a fresh imposter,
    /// and asserts deterministic `_rift.fault` outcomes. Off by default (safe-skip preserved).
    #[arg(long)]
    verify_dynamic: bool,
}

// ============================================================================
// API Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct RootResponse {
    #[serde(default)]
    imposters: Option<Vec<ImposterLink>>,
}

#[derive(Debug, Deserialize)]
struct ImposterLink {
    port: u16,
    protocol: String,
    #[serde(rename = "_links")]
    links: Option<HashMap<String, LinkInfo>>,
}

#[derive(Debug, Deserialize)]
struct LinkInfo {
    href: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImposterDetails {
    port: u16,
    protocol: String,
    name: Option<String>,
    #[serde(default)]
    stubs: Vec<Stub>,
    /// Raw `flowState` block (issue #223). Parsed loosely so the verifier doesn't couple to the
    /// engine's full config schema — we only need `mountebankStateMapping.flowIdSource`.
    #[serde(default)]
    flow_state: Option<serde_json::Value>,
}

impl ImposterDetails {
    /// The header name carrying the correlation/space id, when this imposter isolates flows by a
    /// request header (`flowIdSource: "header:<Name>"`). `None` for the default `imposter_port`.
    fn flow_header(&self) -> Option<String> {
        self.flow_state
            .as_ref()?
            .get("mountebankStateMapping")?
            .get("flowIdSource")?
            .as_str()
            .and_then(flow_id_header_name)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stub {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    predicates: Vec<serde_json::Value>,
    #[serde(default)]
    responses: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct TestCase {
    stub_index: usize,
    stub_id: Option<String>,
    method: String,
    path: String,
    headers: HashMap<String, String>,
    query_params: HashMap<String, String>,
    body: Option<String>,
    expected_status: u16,
    expected_headers: HashMap<String, String>,
    expected_body: Option<serde_json::Value>,
    is_dynamic: bool,
    skip_reason: Option<String>,
    /// Stub is designed to never match (contains "DONT MATCH" or similar in predicates)
    is_no_match_stub: bool,
    /// Imposter protocol ("http"/"https") — selects the request scheme (issue #206).
    protocol: String,
    /// Stub injects a `_rift.fault.tcp` reset (issue #239): a connection error is the expected
    /// outcome, not a failure (finding #3).
    expects_transport_error: bool,
    /// Correlated-isolation header name to send (issue #223), value = `--space`.
    flow_header: Option<String>,
}

#[derive(Debug)]
struct TestResult {
    test_case: TestCase,
    success: bool,
    actual_status: Option<u16>,
    actual_headers: Option<HashMap<String, String>>,
    actual_body: Option<String>,
    error: Option<String>,
    duration_ms: u128,
    failure_reasons: Vec<FailureReason>,
}

#[derive(Debug, Default)]
struct VerificationSummary {
    total_imposters: usize,
    total_stubs: usize,
    total_tests: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    failures: Vec<FailureDetails>,
}

/// Categorizes the specific reason why a verification failed
#[derive(Debug)]
enum FailureReason {
    /// HTTP request failed (connection refused, timeout, etc.)
    RequestError(String),
    /// Status code mismatch
    StatusMismatch { expected: u16, actual: u16 },
    /// Expected header is missing from the response
    HeaderMissing { header_name: String },
    /// Header value doesn't match
    HeaderMismatch {
        header_name: String,
        expected: String,
        actual: String,
    },
    /// Response body doesn't match expected
    BodyMismatch { expected: String, actual: String },
    /// Expected body but got none
    BodyMissing { expected: String },
    /// A `_rift.fault.tcp` stub answered with an HTTP response instead of resetting the
    /// connection — the fault did not fire (issue #249 finding #3).
    TransportResetExpected { actual: u16 },
}

impl FailureReason {
    /// Returns a human-readable hint explaining what went wrong
    fn hint(&self) -> String {
        match self {
            FailureReason::RequestError(err) => {
                if err.contains("Connection refused") {
                    "Hint: The imposter may not be running. Check that Rift is started and the imposter is created.".to_string()
                } else if err.contains("timed out") {
                    "Hint: Request timed out. The server may be slow or unresponsive. Try increasing --timeout.".to_string()
                } else {
                    format!("Hint: HTTP request failed - {err}")
                }
            }
            FailureReason::StatusMismatch { expected, actual } => {
                match *actual {
                    404 => format!("Hint: Got 404 instead of {expected}. The stub predicate may not match the test request path/method."),
                    500 => format!("Hint: Got 500 instead of {expected}. Check server logs for errors."),
                    _ => format!("Hint: Expected status {expected} but got {actual}. Verify the stub response configuration."),
                }
            }
            FailureReason::HeaderMissing { header_name } => {
                format!("Hint: Expected header '{header_name}' is missing from the response. Add it to the stub's response headers.")
            }
            FailureReason::HeaderMismatch { header_name, expected, actual } => {
                format!("Hint: Header '{header_name}' has wrong value.\n       Expected: \"{expected}\"\n       Actual:   \"{actual}\"")
            }
            FailureReason::BodyMismatch { .. } => {
                "Hint: Response body doesn't match. See diff below for details.".to_string()
            }
            FailureReason::BodyMissing { .. } => {
                "Hint: Expected a response body but got an empty response.".to_string()
            }
            FailureReason::TransportResetExpected { actual } => {
                format!("Hint: This stub injects a _rift.fault.tcp reset, so the connection should drop; instead it answered HTTP {actual}. The fault did not fire.")
            }
        }
    }
}

#[derive(Debug)]
struct FailureDetails {
    imposter_port: u16,
    imposter_name: Option<String>,
    stub_index: usize,
    stub_id: Option<String>,
    test_description: String,
    expected: String,
    actual: String,
    curl_command: Option<String>,
    failure_reasons: Vec<FailureReason>,
}

// ============================================================================
// Main Logic
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let client = Client::builder()
        .timeout(Duration::from_secs(args.timeout))
        .danger_accept_invalid_certs(args.insecure)
        .build()?;

    // Check if demo mode
    if args.demo {
        demo_enhanced_error_output();
        return Ok(());
    }

    println!("{BOLD}{CYAN}Rift Stub Verifier{RESET}");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Admin URL: {}", args.admin_url);
    println!();

    // Fetch imposters
    let imposters = fetch_imposters(&client, &args.admin_url, args.port).await?;

    if imposters.is_empty() {
        println!("{YELLOW}Warning:{RESET} No imposters found");
        return Ok(());
    }

    let mut summary = VerificationSummary {
        total_imposters: imposters.len(),
        ..Default::default()
    };

    // Process each imposter
    for imposter in &imposters {
        println!(
            "{}Imposter:{} {} (port {})",
            BOLD,
            RESET,
            imposter.name.as_deref().unwrap_or("unnamed"),
            imposter.port
        );

        summary.total_stubs += imposter.stubs.len();

        if imposter.stubs.is_empty() {
            println!("   └─ No stubs defined");
            println!();
            continue;
        }

        let flow_header = imposter.flow_header();
        for (stub_index, stub) in imposter.stubs.iter().enumerate() {
            let test_cases = generate_test_cases(
                stub_index,
                stub,
                args.skip_dynamic,
                &imposter.protocol,
                flow_header.as_deref(),
            );
            summary.total_tests += test_cases.len();

            for test_case in test_cases {
                if args.show_curl || args.verbose {
                    let curl = generate_curl_command(imposter.port, &test_case);
                    println!("   {DIM}{curl}{RESET}");
                }

                if let Some(reason) = &test_case.skip_reason {
                    // No-match stubs count as passed (they pass by design)
                    // Other skipped stubs (dynamic, etc.) count as skipped
                    if test_case.is_no_match_stub {
                        summary.passed += 1;
                        if args.verbose {
                            println!(
                                "   {}PASS{} Stub #{} - {} {} ({})",
                                GREEN, RESET, stub_index, test_case.method, test_case.path, reason
                            );
                        }
                    } else {
                        summary.skipped += 1;
                        if args.verbose {
                            println!(
                                "   {}SKIP{} Stub #{} - {}",
                                YELLOW,
                                RESET,
                                stub_index,
                                test_case.skip_reason.as_ref().unwrap()
                            );
                        }
                    }
                    continue;
                }

                if args.dry_run {
                    println!(
                        "   {}DRY-RUN{} Stub #{}{} - {} {}",
                        CYAN,
                        RESET,
                        stub_index,
                        test_case
                            .stub_id
                            .as_ref()
                            .map(|id| format!(" [{id}]"))
                            .unwrap_or_default(),
                        test_case.method,
                        test_case.path
                    );
                    summary.skipped += 1;
                    continue;
                }

                let result = execute_test(
                    &client,
                    &args.admin_url,
                    args.gateway,
                    &args.space,
                    imposter.port,
                    &test_case,
                    args.status_only,
                )
                .await;

                if result.success {
                    summary.passed += 1;
                    if args.verbose {
                        println!(
                            "   {}PASS{} Stub #{}{} - {} {} -> {} ({}ms)",
                            GREEN,
                            RESET,
                            stub_index,
                            test_case
                                .stub_id
                                .as_ref()
                                .map(|id| format!(" [{id}]"))
                                .unwrap_or_default(),
                            test_case.method,
                            test_case.path,
                            result.actual_status.unwrap_or(0),
                            result.duration_ms
                        );
                    }
                } else {
                    summary.failed += 1;
                    let failure = FailureDetails {
                        imposter_port: imposter.port,
                        imposter_name: imposter.name.clone(),
                        stub_index,
                        stub_id: test_case.stub_id.clone(),
                        test_description: format!("{} {}", test_case.method, test_case.path),
                        expected: format!(
                            "status={}, body={:?}",
                            test_case.expected_status, test_case.expected_body
                        ),
                        actual: if let Some(err) = &result.error {
                            format!("error: {err}")
                        } else {
                            format!(
                                "status={}, body={:?}",
                                result.actual_status.unwrap_or(0),
                                result.actual_body
                            )
                        },
                        curl_command: Some(generate_curl_command(imposter.port, &test_case)),
                        failure_reasons: result.failure_reasons,
                    };

                    println!(
                        "   {}FAIL{} Stub #{}{} - {} {}",
                        RED,
                        RESET,
                        stub_index,
                        test_case
                            .stub_id
                            .as_ref()
                            .map(|id| format!(" [{id}]"))
                            .unwrap_or_default(),
                        test_case.method,
                        test_case.path
                    );

                    // Show enhanced error details inline when verbose
                    if args.verbose && !failure.failure_reasons.is_empty() {
                        println!("   {BOLD}Why it failed:{RESET}");
                        for reason in &failure.failure_reasons {
                            print_failure_reason(reason);
                        }
                    }

                    summary.failures.push(failure);
                }
            }
        }
        println!();
    }

    // Opt-in dynamic-behavior assertion (issue #251): proxy / `_verify` sequences / faults.
    if args.verify_dynamic {
        run_dynamic_verification(&client, &args.admin_url, &imposters, &mut summary).await;
    }

    // Print summary
    print_summary(&summary, args.show_curl);

    // Exit with error code if any failures
    if summary.failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Run the opt-in dynamic-behavior assertions (issue #251) for every imposter and fold the
/// resulting checks into the summary. Operates on the raw `GET /imposters/:port` JSON so all
/// engine-preserved fields (`_verify`, `proxy`, `_rift.fault`) are visible.
async fn run_dynamic_verification(
    client: &Client,
    admin_url: &str,
    imposters: &[ImposterDetails],
    summary: &mut VerificationSummary,
) {
    let verifier = dynamic::DynamicVerifier { client, admin_url };
    println!();
    println!("{BOLD}{CYAN}Dynamic assertions (--verify-dynamic){RESET}");

    for imposter in imposters {
        // The imposter list was already fetched successfully, so a per-imposter GET/parse failure
        // here is anomalous — its dynamic checks could not run. Count it as a FAILURE (visible in
        // the exit code) rather than a silent skip that still exits 0.
        let fetch = client
            .get(format!("{admin_url}/imposters/{}", imposter.port))
            .send()
            .await
            .map_err(|e| format!("fetch: {e}"));
        let raw: serde_json::Value = match fetch {
            Ok(resp) => match resp.json().await {
                Ok(value) => value,
                Err(e) => {
                    record_dynamic_fetch_failure(summary, imposter, format!("parse: {e}"));
                    continue;
                }
            },
            Err(e) => {
                record_dynamic_fetch_failure(summary, imposter, e);
                continue;
            }
        };

        for check in verifier.verify_imposter(&raw).await {
            if check.skipped {
                summary.skipped += 1;
                println!("   {YELLOW}SKIP{RESET} {} — {}", check.label, check.detail);
            } else if check.passed {
                summary.passed += 1;
                println!("   {GREEN}PASS{RESET} {}", check.label);
            } else {
                summary.failed += 1;
                println!("   {RED}FAIL{RESET} {} — {}", check.label, check.detail);
                summary.failures.push(FailureDetails {
                    imposter_port: imposter.port,
                    imposter_name: imposter.name.clone(),
                    stub_index: 0,
                    stub_id: None,
                    test_description: check.label,
                    expected: "dynamic assertion".to_string(),
                    actual: check.detail,
                    curl_command: None,
                    failure_reasons: vec![],
                });
            }
        }
    }
}

/// Record a per-imposter dynamic-fetch failure as a verification failure (issue #251): a verifier
/// that can't even read an imposter must not report success for it.
fn record_dynamic_fetch_failure(
    summary: &mut VerificationSummary,
    imposter: &ImposterDetails,
    detail: String,
) {
    summary.failed += 1;
    println!(
        "   {RED}FAIL{RESET} imposter {} dynamic fetch — {detail}",
        imposter.port
    );
    summary.failures.push(FailureDetails {
        imposter_port: imposter.port,
        imposter_name: imposter.name.clone(),
        stub_index: 0,
        stub_id: None,
        test_description: "dynamic fetch".to_string(),
        expected: "imposter detail fetched for dynamic assertion".to_string(),
        actual: detail,
        curl_command: None,
        failure_reasons: vec![],
    });
}

// ============================================================================
// Imposter Fetching
// ============================================================================

async fn fetch_imposters(
    client: &Client,
    admin_url: &str,
    filter_port: Option<u16>,
) -> Result<Vec<ImposterDetails>, Box<dyn std::error::Error>> {
    // Get list of imposters
    let imposters_url = format!("{admin_url}/imposters");
    let response = client.get(&imposters_url).send().await?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to fetch imposters: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        )
        .into());
    }

    let imposters_response: serde_json::Value = response.json().await?;

    // Handle both formats: { imposters: [...] } and { imposters: [...], ... }
    let imposter_links: Vec<ImposterLink> =
        if let Some(imposters) = imposters_response.get("imposters") {
            serde_json::from_value(imposters.clone())?
        } else {
            vec![]
        };

    let mut imposters = Vec::new();

    for link in imposter_links {
        if let Some(port) = filter_port {
            if link.port != port {
                continue;
            }
        }

        // Fetch full imposter details
        let detail_url = format!("{}/imposters/{}", admin_url, link.port);
        let detail_response = client.get(&detail_url).send().await?;

        if detail_response.status().is_success() {
            let details: ImposterDetails = detail_response.json().await?;
            imposters.push(details);
        }
    }

    Ok(imposters)
}

// ============================================================================
// Test Case Generation
// ============================================================================

fn generate_test_cases(
    stub_index: usize,
    stub: &Stub,
    skip_dynamic: bool,
    protocol: &str,
    flow_header: Option<&str>,
) -> Vec<TestCase> {
    let mut test_cases = Vec::new();

    // Check if this stub has dynamic responses
    let (is_dynamic, dynamic_type) = check_if_dynamic(&stub.responses);

    // A `_rift.fault.tcp` stub resets the connection (issue #239): a transport error is the
    // expected outcome rather than a failure (finding #3).
    let expects_transport_error = expects_tcp_fault(&stub.responses);

    // Check if this stub is designed to never match
    let is_no_match_stub = check_if_no_match_stub(&stub.predicates);

    // Parse predicates to build test request (needed for all cases)
    let (method, path, headers, query_params, body) = parse_predicates(&stub.predicates);
    let flow_header = flow_header.map(str::to_string);

    // No-match stubs (e.g., "DONT MATCH THIS") are designed to never match any request.
    // We mark them as passed because:
    // 1. Testing them would hit other broader stubs that DO match the path
    // 2. Their purpose is to ensure they don't accidentally match real traffic
    // 3. Their existence in the config is the test - they pass by design
    if is_no_match_stub {
        test_cases.push(TestCase {
            stub_index,
            stub_id: stub.id.clone(),
            method,
            path,
            headers,
            query_params,
            body,
            expected_status: 200,
            expected_headers: HashMap::new(),
            expected_body: None,
            is_dynamic: false,
            skip_reason: Some("no-match stub (passes by design)".to_string()),
            is_no_match_stub: true,
            protocol: protocol.to_string(),
            expects_transport_error: false,
            flow_header: flow_header.clone(),
        });
        return test_cases;
    }

    // If skipping dynamic and this is dynamic, mark as skipped
    if is_dynamic && skip_dynamic {
        test_cases.push(TestCase {
            stub_index,
            stub_id: stub.id.clone(),
            method,
            path,
            headers,
            query_params,
            body,
            expected_status: 200,
            expected_headers: HashMap::new(),
            expected_body: None,
            is_dynamic: true,
            skip_reason: dynamic_type,
            is_no_match_stub: false,
            protocol: protocol.to_string(),
            expects_transport_error,
            flow_header: flow_header.clone(),
        });
        return test_cases;
    }

    // Extract expected response from first response
    let (expected_status, expected_headers, expected_body) =
        extract_expected_response(&stub.responses);

    test_cases.push(TestCase {
        stub_index,
        stub_id: stub.id.clone(),
        method,
        path,
        headers,
        query_params,
        body,
        expected_status,
        expected_headers,
        expected_body,
        is_dynamic,
        skip_reason: None,
        is_no_match_stub: false,
        protocol: protocol.to_string(),
        expects_transport_error,
        flow_header,
    });

    test_cases
}

/// Check if a stub's predicates contain patterns indicating it should never match.
/// These stubs typically have paths like "DONT MATCH THIS" or "DO NOT MATCH THIS"
/// to ensure they never match actual requests.
fn check_if_no_match_stub(predicates: &[serde_json::Value]) -> bool {
    let no_match_patterns = [
        "DONT MATCH",
        "DO NOT MATCH",
        "NEVER MATCH",
        "NO MATCH",
        "NOMATCH",
    ];

    for predicate in predicates {
        // Check in equals, contains, startsWith, endsWith predicates
        for key in ["equals", "contains", "startsWith", "endsWith", "deepEquals"] {
            if let Some(pred) = predicate.get(key) {
                // Check path field
                if let Some(path) = pred.get("path").and_then(|v| v.as_str()) {
                    let path_upper = path.to_uppercase();
                    for pattern in &no_match_patterns {
                        if path_upper.contains(pattern) {
                            return true;
                        }
                    }
                }
                // Check body field
                if let Some(body) = pred.get("body").and_then(|v| v.as_str()) {
                    let body_upper = body.to_uppercase();
                    for pattern in &no_match_patterns {
                        if body_upper.contains(pattern) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

fn check_if_dynamic(responses: &[serde_json::Value]) -> (bool, Option<String>) {
    if responses.is_empty() {
        return (false, None);
    }

    // Multiple responses = cycling behavior (stateful, can't predict which response)
    if responses.len() > 1 {
        return (
            true,
            Some(format!("cycling responses ({} responses)", responses.len())),
        );
    }

    let first = &responses[0];

    if first.get("inject").is_some() {
        return (true, Some("inject response (JavaScript)".to_string()));
    }

    // Only treat as proxy if it's a real proxy config (object with "to" field)
    // Many stubs have "proxy": null which should not be treated as dynamic
    if let Some(proxy) = first.get("proxy") {
        if proxy.is_object() && proxy.get("to").is_some() {
            return (true, Some("proxy response".to_string()));
        }
    }

    if first.get("fault").is_some() {
        return (true, Some("fault injection".to_string()));
    }

    // Check for _rift script extension and _rift.fault (tcp/latency/error are non-deterministic
    // or transport-level; can't be asserted as a normal HTTP response).
    if let Some(rift) = first.get("_rift") {
        if rift.get("script").is_some() {
            return (true, Some("Rift script response".to_string()));
        }
        if rift.get("fault").is_some() {
            return (true, Some("Rift fault (_rift.fault)".to_string()));
        }
    }

    // Behaviors whose output depends on the request or external state can't be predicted from
    // the stub alone (repeat=stateful; decorate/copy/lookup/shellTransform=dynamic body/headers).
    // Handle BOTH the input config form (`_behaviors` object) and the form returned by
    // GET /imposters (`behaviors` array of single-key objects, Mountebank-style).
    let label = |k: &str| match k {
        "repeat" => "repeat behavior (stateful)",
        "decorate" => "decorate behavior (dynamic)",
        "copy" => "copy behavior (request-derived)",
        "lookup" => "lookup behavior (data-source)",
        "shellTransform" => "shellTransform behavior (external)",
        _ => "dynamic behavior",
    };
    const DYNAMIC_BEHAVIORS: [&str; 5] = ["repeat", "decorate", "copy", "lookup", "shellTransform"];
    if let Some(obj) = first.get("_behaviors").and_then(|v| v.as_object()) {
        for k in DYNAMIC_BEHAVIORS {
            if obj.contains_key(k) {
                return (true, Some(label(k).to_string()));
            }
        }
    }
    if let Some(arr) = first.get("behaviors").and_then(|v| v.as_array()) {
        for item in arr.iter().filter_map(|v| v.as_object()) {
            for k in DYNAMIC_BEHAVIORS {
                if item.contains_key(k) {
                    return (true, Some(label(k).to_string()));
                }
            }
        }
    }

    (false, None)
}

/// True when the stub's first response injects a connection-level TCP fault
/// (`_rift.fault.tcp`, issue #239), for which a transport error is the expected outcome.
fn expects_tcp_fault(responses: &[serde_json::Value]) -> bool {
    responses
        .first()
        .and_then(|r| r.get("_rift"))
        .and_then(|r| r.get("fault"))
        .and_then(|f| f.get("tcp"))
        .is_some()
}

/// Classify a request error string as a connection-level reset/abort (the signature of a TCP
/// fault) rather than an application error. Used to PASS `_rift.fault.tcp` stubs (finding #3).
fn is_transport_reset_error(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("connection reset")
        || e.contains("connection closed")
        || e.contains("connection aborted")
        || e.contains("broken pipe")
        || e.contains("incomplete message")
}

/// A `_rift.fault.tcp` stub passes when (and only when) the request fails with a connection-level
/// reset. Scoping to `expects_transport_error` keeps normal stubs failing on any connection error.
fn is_expected_reset(expects_transport_error: bool, error_msg: &str) -> bool {
    expects_transport_error && is_transport_reset_error(error_msg)
}

/// Build the request URL for a stub. With `--gateway`, route through the single admin port
/// (`{admin_url}/__rift/<port>/...`, issue #212); otherwise connect to the imposter port directly,
/// choosing the scheme from the imposter protocol (`https` for issue #206).
fn build_target_url(
    admin_url: &str,
    gateway: bool,
    protocol: &str,
    port: u16,
    path: &str,
) -> String {
    if gateway {
        let base = admin_url.trim_end_matches('/');
        return format!("{base}/__rift/{port}{path}");
    }
    let scheme = if protocol.eq_ignore_ascii_case("https") {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://localhost:{port}{path}")
}

/// Extract the correlation header name from a `flowIdSource` value (issue #223): `"header:X-Foo"`
/// yields `Some("X-Foo")`; `"imposter_port"` (or anything without the prefix) yields `None`.
fn flow_id_header_name(flow_id_source: &str) -> Option<String> {
    flow_id_source
        .strip_prefix("header:")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn extract_expected_response(
    responses: &[serde_json::Value],
) -> (u16, HashMap<String, String>, Option<serde_json::Value>) {
    if responses.is_empty() {
        return (200, HashMap::new(), None);
    }

    let first = &responses[0];

    // Check if this has an "is" response - this takes priority over proxy
    // Many stubs have "proxy": null alongside "is", so we should use "is" when present
    let has_is_response = first.get("is").is_some();

    // Handle proxy response - only if it's a real proxy config (not null) and there's no "is" response
    if !has_is_response {
        if let Some(proxy) = first.get("proxy") {
            // proxy must be an object with a "to" field to be a real proxy
            if proxy.is_object() && proxy.get("to").is_some() {
                // For proxy, we just verify connectivity - any 2xx is fine, no specific body expected
                return (200, HashMap::new(), None);
            }
        }
    }

    // Handle inject response - expect any response from the JavaScript
    if first.get("inject").is_some() {
        return (200, HashMap::new(), None);
    }

    // Handle fault response
    if let Some(fault) = first.get("fault") {
        // If fault has a specific status, use that
        if let Some(status) = fault.get("status").and_then(|v| v.as_u64()) {
            return (status as u16, HashMap::new(), None);
        }
        // Default fault behavior might return connection errors, but we can expect 500
        return (500, HashMap::new(), None);
    }

    // Handle "is" response format
    if let Some(is_response) = first.get("is") {
        let status = is_response
            .get("statusCode")
            .and_then(|v| {
                // Try as number first, then as string
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(200) as u16;

        let headers = is_response
            .get("headers")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let body = is_response.get("body").cloned();

        return (status, headers, body);
    }

    // Direct format without "is" wrapper
    let status = first
        .get("statusCode")
        .and_then(|v| {
            // Try as number first, then as string
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(200) as u16;

    let headers = first
        .get("headers")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let body = first.get("body").cloned();

    (status, headers, body)
}

#[allow(clippy::type_complexity)]
fn parse_predicates(
    predicates: &[serde_json::Value],
) -> (
    String,
    String,
    HashMap<String, String>,
    HashMap<String, String>,
    Option<String>,
) {
    let mut method = "GET".to_string();
    let mut path = "/".to_string();
    let mut headers = HashMap::new();
    let mut query_params = HashMap::new();
    let mut body = None;
    let mut jsonpath_body: Option<serde_json::Value> = None;

    // First pass: extract startsWith to set base path (regardless of predicate order)
    for predicate in predicates {
        if let Some(starts_with) = predicate.get("startsWith") {
            if let Some(p) = starts_with.get("path").and_then(|v| v.as_str()) {
                path = p.to_string();
            }
        }
    }

    // Second pass: process all other predicates
    for predicate in predicates {
        // Handle jsonpath predicates - build a JSON body based on the selector
        if let Some(jsonpath) = predicate.get("jsonpath") {
            if let Some(selector) = jsonpath.get("selector").and_then(|v| v.as_str()) {
                // Get the expected value from equals.body
                if let Some(equals) = predicate.get("equals") {
                    if let Some(value) = equals.get("body") {
                        let json_value = if let Some(s) = value.as_str() {
                            serde_json::Value::String(s.to_string())
                        } else {
                            value.clone()
                        };

                        // Build or merge into jsonpath_body
                        let new_obj = build_json_from_jsonpath(selector, json_value);
                        jsonpath_body = Some(match jsonpath_body {
                            Some(existing) => merge_json_objects(existing, new_obj),
                            None => new_obj,
                        });
                    }
                }
            }
        }
        // Handle xpath predicates - build a matching XML body from the selector and the
        // expected `equals.body` value (mirrors the jsonpath handling above). Issue #249 finding #1.
        if let Some(xpath) = predicate.get("xpath") {
            if let Some(selector) = xpath.get("selector").and_then(|v| v.as_str()) {
                if let Some(value) = predicate
                    .get("equals")
                    .and_then(|e| e.get("body"))
                    .and_then(|v| v.as_str())
                {
                    body = Some(build_xml_from_xpath(selector, value));
                }
            }
        }

        // Handle various predicate formats
        // Note: startsWith is already processed in first pass

        // "equals" predicate - skip body when handled by jsonpath/xpath above
        if let Some(equals) = predicate.get("equals") {
            let skip_body = predicate.get("jsonpath").is_some() || predicate.get("xpath").is_some();
            parse_equals_predicate(
                equals,
                &mut method,
                &mut path,
                &mut headers,
                &mut query_params,
                &mut body,
                skip_body,
            );
        }

        // "matches" predicate (regex - use a sample value)
        if let Some(matches) = predicate.get("matches") {
            if let Some(p) = matches.get("path").and_then(|v| v.as_str()) {
                // Generate a sample path that might match the regex
                path = generate_sample_from_regex(p);
            }
            if let Some(m) = matches.get("method").and_then(|v| v.as_str()) {
                method = generate_sample_from_regex(m);
            }
        }

        // "exists" predicate
        if let Some(exists) = predicate.get("exists") {
            if let Some(hdrs) = exists.get("headers").and_then(|v| v.as_object()) {
                for (name, should_exist) in hdrs {
                    if should_exist.as_bool().unwrap_or(true) {
                        headers.insert(name.clone(), "test-value".to_string());
                    }
                }
            }
        }

        // "deepEquals" predicate - skip body when handled by jsonpath/xpath above
        if let Some(deep_equals) = predicate.get("deepEquals") {
            let skip_body = predicate.get("jsonpath").is_some() || predicate.get("xpath").is_some();
            parse_equals_predicate(
                deep_equals,
                &mut method,
                &mut path,
                &mut headers,
                &mut query_params,
                &mut body,
                skip_body,
            );
        }

        // "contains" predicate - processed after base path is set
        if let Some(contains) = predicate.get("contains") {
            parse_contains_predicate(
                contains,
                &mut path,
                &mut headers,
                &mut body,
                &mut query_params,
            );
        }

        // "endsWith" predicate - append to path if needed
        if let Some(ends_with) = predicate.get("endsWith") {
            if let Some(p) = ends_with.get("path").and_then(|v| v.as_str()) {
                // If path doesn't end with the required suffix, append it
                if !path.ends_with(p) {
                    if path == "/" {
                        path = format!("/prefix{p}");
                    } else if !path.ends_with('/') && !p.starts_with('/') {
                        path = format!("{path}/{p}");
                    } else {
                        path = format!("{path}{p}");
                    }
                }
            }
        }

        // "and" predicate - recursively parse all inner predicates
        if let Some(and_predicates) = predicate.get("and").and_then(|v| v.as_array()) {
            let inner: Vec<serde_json::Value> = and_predicates.clone();
            let (m, p, h, q, b) = parse_predicates(&inner);
            if m != "GET" {
                method = m;
            }
            if p != "/" {
                path = p;
            }
            headers.extend(h);
            query_params.extend(q);
            if b.is_some() {
                body = b;
            }
        }

        // "or" predicate - use first inner predicate
        if let Some(or_predicates) = predicate.get("or").and_then(|v| v.as_array()) {
            if let Some(first) = or_predicates.first() {
                let inner = vec![first.clone()];
                let (m, p, h, q, b) = parse_predicates(&inner);
                if m != "GET" {
                    method = m;
                }
                if p != "/" {
                    path = p;
                }
                headers.extend(h);
                query_params.extend(q);
                if b.is_some() {
                    body = b;
                }
            }
        }

        // "not" predicate - the stub matches when the inner predicate is FALSE, so build a
        // request that deliberately violates it (issue #249 finding #1).
        if let Some(inner) = predicate.get("not") {
            apply_not_predicate(inner, &mut method, &mut path);
        }
    }

    // If we built a jsonpath body and no explicit body was set, use it
    if body.is_none() && jsonpath_body.is_some() {
        body = jsonpath_body.map(|v| serde_json::to_string(&v).unwrap_or_default());
    }

    (method, path, headers, query_params, body)
}

/// Mutate the working request so it does NOT satisfy `inner` (the body of a `not` predicate).
/// We parse what WOULD satisfy `inner`, then steer each field the inner predicate actually
/// constrains to a guaranteed-different value. Path and method cover the predicates the verifier
/// generates requests from; an inner constraint on body/headers/query is already violated by the
/// default empty request.
fn apply_not_predicate(inner: &serde_json::Value, method: &mut String, path: &mut String) {
    let (inner_method, _inner_path, _h, _q, _b) = parse_predicates(std::slice::from_ref(inner));
    if predicate_mentions_field(inner, "path") {
        // A sentinel path won't equal / start-with / match a real constrained path (including "/").
        *path = "/__rift_verify_no_match__".to_string();
    }
    if predicate_mentions_field(inner, "method") {
        // Pick any method other than the one the inner predicate constrains (its default is GET).
        *method = if inner_method.eq_ignore_ascii_case("GET") {
            "DELETE".to_string()
        } else {
            "GET".to_string()
        };
    }
}

/// Recursively report whether any object inside `value` carries the key `field` (used to detect
/// which request fields a `not` predicate actually constrains, regardless of operator/nesting).
fn predicate_mentions_field(value: &serde_json::Value, field: &str) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key(field) || map.values().any(|v| predicate_mentions_field(v, field))
        }
        serde_json::Value::Array(items) => items.iter().any(|v| predicate_mentions_field(v, field)),
        _ => false,
    }
}

/// Build a matching XML body from an xpath selector and a leaf value, mirroring
/// `build_json_from_jsonpath`. e.g. ("/order/id", "728839") -> "<order><id>728839</id></order>".
fn build_xml_from_xpath(selector: &str, value: &str) -> String {
    let parts: Vec<&str> = selector.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return value.to_string();
    }
    let mut xml = value.to_string();
    for part in parts.iter().rev() {
        // Strip positional predicates / attribute markers (e.g. `item[1]`, `@id`) to the bare tag.
        let tag = part
            .split('[')
            .next()
            .unwrap_or(part)
            .trim_start_matches('@');
        xml = format!("<{tag}>{xml}</{tag}>");
    }
    xml
}

/// Build a JSON object from a jsonpath selector and value
/// e.g., "$.receiver.context.correlationKeys.[:0].keyValue" with value "728839"
/// becomes {"receiver":{"context":{"correlationKeys":[{"keyValue":"728839"}]}}}
fn build_json_from_jsonpath(selector: &str, value: serde_json::Value) -> serde_json::Value {
    // Remove leading $. if present
    let path = selector.strip_prefix("$.").unwrap_or(selector);

    // Split by . and build nested structure
    let parts: Vec<&str> = path.split('.').collect();

    // Build from inside out
    let mut result = value;

    for part in parts.iter().rev() {
        if part.starts_with("[:") || part.starts_with("[") {
            // Array index like "[:0]" or "[0]" - wrap in array
            result = serde_json::json!([result]);
        } else {
            // Object key
            let mut obj = serde_json::Map::new();
            obj.insert((*part).to_string(), result);
            result = serde_json::Value::Object(obj);
        }
    }

    result
}

/// Merge two JSON objects recursively
fn merge_json_objects(
    mut base: serde_json::Value,
    overlay: serde_json::Value,
) -> serde_json::Value {
    if let (serde_json::Value::Object(base_obj), serde_json::Value::Object(overlay_obj)) =
        (&mut base, &overlay)
    {
        for (key, value) in overlay_obj {
            if let Some(existing) = base_obj.get_mut(key) {
                *existing = merge_json_objects(existing.clone(), value.clone());
            } else {
                base_obj.insert(key.clone(), value.clone());
            }
        }
        base
    } else if let (serde_json::Value::Array(base_arr), serde_json::Value::Array(overlay_arr)) =
        (&mut base, &overlay)
    {
        // Merge arrays by extending or merging first elements
        if !overlay_arr.is_empty() {
            if base_arr.is_empty() {
                base_arr.extend(overlay_arr.clone());
            } else {
                // Merge first elements if both are objects
                let merged = merge_json_objects(base_arr[0].clone(), overlay_arr[0].clone());
                base_arr[0] = merged;
            }
        }
        base
    } else {
        overlay
    }
}

fn parse_equals_predicate(
    equals: &serde_json::Value,
    method: &mut String,
    path: &mut String,
    headers: &mut HashMap<String, String>,
    query_params: &mut HashMap<String, String>,
    body: &mut Option<String>,
    skip_body: bool,
) {
    if let Some(m) = equals.get("method").and_then(|v| v.as_str()) {
        *method = m.to_string();
    }

    if let Some(p) = equals.get("path").and_then(|v| v.as_str()) {
        *path = p.to_string();
    }

    if let Some(hdrs) = equals.get("headers").and_then(|v| v.as_object()) {
        for (name, value) in hdrs {
            if let Some(v) = value.as_str() {
                headers.insert(name.clone(), v.to_string());
            }
        }
    }

    if let Some(query) = equals.get("query").and_then(|v| v.as_object()) {
        for (name, value) in query {
            if let Some(v) = value.as_str() {
                query_params.insert(name.clone(), v.to_string());
            }
        }
    }

    // Skip body if it's being handled by jsonpath
    if !skip_body {
        if let Some(b) = equals.get("body") {
            if let Some(s) = b.as_str() {
                // Don't set body if it's an empty string (means "body should be absent")
                if !s.is_empty() {
                    *body = Some(s.to_string());
                }
            } else {
                *body = Some(serde_json::to_string(b).unwrap_or_default());
            }
        }
    }
}

fn parse_contains_predicate(
    contains: &serde_json::Value,
    path: &mut String,
    headers: &mut HashMap<String, String>,
    body: &mut Option<String>,
    query_params: &mut HashMap<String, String>,
) {
    // For "contains", we need to include the substring in our test value
    if let Some(p) = contains.get("path").and_then(|v| v.as_str()) {
        // If path already has a value from startsWith/equals, append to it
        // Otherwise, use the contains value as the path (prefixing / if needed)
        if *path == "/" {
            if p.starts_with('/') {
                *path = p.to_string();
            } else {
                *path = format!("/{p}");
            }
        } else if !path.contains(p) {
            // Append the contains substring to the existing path if not already present
            // Add a slash separator if needed
            if !path.ends_with('/') && !p.starts_with('/') {
                path.push('/');
            }
            path.push_str(p);
        }
    }

    // Handle query parameters in contains
    if let Some(query) = contains.get("query").and_then(|v| v.as_object()) {
        for (name, value) in query {
            if let Some(v) = value.as_str() {
                // For contains, include the substring in the query value
                query_params.insert(name.clone(), v.to_string());
            }
        }
    }

    if let Some(hdrs) = contains.get("headers").and_then(|v| v.as_object()) {
        for (name, value) in hdrs {
            if let Some(v) = value.as_str() {
                headers.insert(name.clone(), format!("prefix{v}suffix"));
            }
        }
    }

    if let Some(b) = contains.get("body").and_then(|v| v.as_str()) {
        // Append to existing body if present (handles multiple contains predicates)
        if let Some(existing) = body {
            *body = Some(format!("{existing} {b}"));
        } else {
            *body = Some(format!("test {b} content"));
        }
    }
}

fn generate_sample_from_regex(pattern: &str) -> String {
    // Simple heuristic to generate a sample that might match common patterns
    // This is a best-effort approach for common regex patterns

    // /api/v\d+/users -> /api/v1/users
    // Important: Replace character class patterns BEFORE stripping anchors,
    // since [^/]+ contains ^ as negation, not as anchor
    let sample = pattern
        // Replace character classes first (before anchor removal)
        .replace(r"[^/]+", "item")
        .replace(r"[a-zA-Z]+", "test")
        .replace(r"[0-9]+", "123")
        .replace(r"[a-z]+", "test")
        .replace(r"[A-Z]+", "TEST")
        // Replace other common patterns
        .replace(r"\d+", "1")
        .replace(r"\d", "1")
        .replace(r"\w+", "test")
        .replace(r"\w", "a")
        .replace(r".*", "")
        .replace(r".+", "x");

    // Strip anchors only at start/end of string
    let sample = sample.strip_prefix('^').unwrap_or(&sample).to_string();
    let sample = sample.strip_suffix('$').unwrap_or(&sample).to_string();

    if sample.is_empty() {
        "/".to_string()
    } else {
        sample
    }
}

// ============================================================================
// Test Execution
// ============================================================================

async fn execute_test(
    client: &Client,
    admin_url: &str,
    gateway: bool,
    space: &str,
    imposter_port: u16,
    test_case: &TestCase,
    status_only: bool,
) -> TestResult {
    let start = std::time::Instant::now();

    // Build URL with query params (gateway/https-aware, issues #212/#206)
    let mut url = build_target_url(
        admin_url,
        gateway,
        &test_case.protocol,
        imposter_port,
        &test_case.path,
    );
    if !test_case.query_params.is_empty() {
        let query_string: Vec<String> = test_case
            .query_params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect();
        url = format!("{}?{}", url, query_string.join("&"));
    }

    // Build request
    let mut request = match test_case.method.to_uppercase().as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        "PATCH" => client.patch(&url),
        "HEAD" => client.head(&url),
        _ => client.get(&url),
    };

    // Add headers
    for (name, value) in &test_case.headers {
        request = request.header(name, value);
    }

    // Correlated-isolation header (issue #223): route this request to the verifier's space.
    if let Some(flow_header) = &test_case.flow_header {
        request = request.header(flow_header, space);
    }

    // Add body if present
    if let Some(ref body) = test_case.body {
        request = request.body(body.clone());
    }

    // Execute request
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let headers: HashMap<String, String> = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|v| (name.as_str().to_string(), v.to_string()))
                })
                .collect();
            let body_text = response.text().await.ok();

            let duration_ms = start.elapsed().as_millis();

            // A `_rift.fault.tcp` stub must reset the connection; receiving any HTTP response means
            // the fault did not fire — the exact regression this verifier exists to catch (finding #3).
            if test_case.expects_transport_error {
                return TestResult {
                    test_case: test_case.clone(),
                    success: false,
                    actual_status: Some(status),
                    actual_headers: Some(headers),
                    actual_body: body_text,
                    error: None,
                    duration_ms,
                    failure_reasons: vec![FailureReason::TransportResetExpected { actual: status }],
                };
            }

            // Verify response
            // If status_only mode, only check status code (no body/header checks)
            let verify_result = if status_only {
                verify_response(
                    test_case.expected_status,
                    &HashMap::new(), // no expected headers
                    &None,           // no expected body
                    status,
                    &headers,
                    &body_text,
                    false, // strict status checking (compare expected vs actual)
                )
            } else {
                verify_response(
                    test_case.expected_status,
                    &test_case.expected_headers,
                    &test_case.expected_body,
                    status,
                    &headers,
                    &body_text,
                    test_case.is_dynamic,
                )
            };

            let success = verify_result.is_success();
            let failure_reasons = verify_result.failure_reasons();

            TestResult {
                test_case: test_case.clone(),
                success,
                actual_status: Some(status),
                actual_headers: Some(headers),
                actual_body: body_text,
                error: None,
                duration_ms,
                failure_reasons,
            }
        }
        Err(e) => {
            let error_msg = e.to_string();
            // A `_rift.fault.tcp` stub is expected to reset the connection (issue #239): a
            // transport-level error is the PASS condition, not a failure (finding #3).
            let expected_reset = is_expected_reset(test_case.expects_transport_error, &error_msg);
            TestResult {
                test_case: test_case.clone(),
                success: expected_reset,
                actual_status: None,
                actual_headers: None,
                actual_body: None,
                error: Some(error_msg.clone()),
                duration_ms: start.elapsed().as_millis(),
                failure_reasons: if expected_reset {
                    vec![]
                } else {
                    vec![FailureReason::RequestError(error_msg)]
                },
            }
        }
    }
}

/// Result of verification - either success or a list of failure reasons
#[derive(Debug)]
enum VerifyResult {
    Success,
    Failed(Vec<FailureReason>),
}

impl VerifyResult {
    fn is_success(&self) -> bool {
        matches!(self, VerifyResult::Success)
    }

    fn failure_reasons(self) -> Vec<FailureReason> {
        match self {
            VerifyResult::Success => vec![],
            VerifyResult::Failed(reasons) => reasons,
        }
    }
}

fn verify_response(
    expected_status: u16,
    expected_headers: &HashMap<String, String>,
    expected_body: &Option<serde_json::Value>,
    actual_status: u16,
    actual_headers: &HashMap<String, String>,
    actual_body: &Option<String>,
    is_dynamic: bool,
) -> VerifyResult {
    let mut failures = Vec::new();

    // Check status code
    // For dynamic responses (proxy, inject), accept any 2xx status
    let status_ok = if is_dynamic {
        (200..300).contains(&actual_status)
    } else {
        expected_status == actual_status
    };

    if !status_ok {
        failures.push(FailureReason::StatusMismatch {
            expected: expected_status,
            actual: actual_status,
        });
    }

    // Check expected headers (actual may have more headers, that's ok)
    for (name, expected_value) in expected_headers {
        let name_lower = name.to_lowercase();
        let actual_value = actual_headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == name_lower)
            .map(|(_, v)| v);

        match actual_value {
            None => {
                failures.push(FailureReason::HeaderMissing {
                    header_name: name.clone(),
                });
            }
            Some(actual) if actual != expected_value => {
                failures.push(FailureReason::HeaderMismatch {
                    header_name: name.clone(),
                    expected: expected_value.clone(),
                    actual: actual.clone(),
                });
            }
            _ => {}
        }
    }

    // Check body if expected
    if let Some(expected) = expected_body {
        match actual_body {
            None => {
                failures.push(FailureReason::BodyMissing {
                    expected: format_json_for_diff(expected),
                });
            }
            Some(actual_text) => {
                // Normalize expected - if it's a string containing JSON, parse it
                let expected_normalized = normalize_json_value(expected);

                // Try to parse actual as JSON
                if let Ok(actual_json) = serde_json::from_str::<serde_json::Value>(actual_text) {
                    // Both are JSON - do semantic comparison
                    if !json_matches(&expected_normalized, &actual_json) {
                        failures.push(FailureReason::BodyMismatch {
                            expected: format_json_for_diff(&expected_normalized),
                            actual: format_json_for_diff(&actual_json),
                        });
                    }
                } else {
                    // Actual is not valid JSON - compare as strings
                    let expected_plain = match &expected_normalized {
                        serde_json::Value::String(s) => s.clone(),
                        _ => expected_normalized.to_string(),
                    };
                    if actual_text != &expected_plain {
                        failures.push(FailureReason::BodyMismatch {
                            expected: expected_plain,
                            actual: actual_text.clone(),
                        });
                    }
                }
            }
        }
    }

    if failures.is_empty() {
        VerifyResult::Success
    } else {
        VerifyResult::Failed(failures)
    }
}

/// Pretty-print JSON for diff display
fn format_json_for_diff(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Normalize a JSON value by parsing string values that contain JSON.
/// This handles cases where the expected body is defined as a string like:
/// `"{\"key\": \"value\"}"` instead of as a proper JSON object.
fn normalize_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            // Try to parse the string as JSON
            serde_json::from_str(s).unwrap_or_else(|_| value.clone())
        }
        _ => value.clone(),
    }
}

/// Checks if two JSON values are semantically equal.
/// This handles:
/// - Different key ordering in objects
/// - Compact vs pretty-printed formatting
/// - String values that contain JSON (parses and compares them)
fn json_matches(expected: &serde_json::Value, actual: &serde_json::Value) -> bool {
    match (expected, actual) {
        (serde_json::Value::Object(exp_obj), serde_json::Value::Object(act_obj)) => {
            // Objects must have the same keys with matching values
            if exp_obj.len() != act_obj.len() {
                return false;
            }
            exp_obj.iter().all(|(key, exp_val)| {
                act_obj
                    .get(key)
                    .map(|act_val| json_matches(exp_val, act_val))
                    .unwrap_or(false)
            })
        }
        (serde_json::Value::Array(exp_arr), serde_json::Value::Array(act_arr)) => {
            exp_arr.len() == act_arr.len()
                && exp_arr
                    .iter()
                    .zip(act_arr.iter())
                    .all(|(e, a)| json_matches(e, a))
        }
        // Handle case where one side is a JSON string that needs parsing
        (serde_json::Value::String(exp_str), actual) => {
            // Try to parse the expected string as JSON
            if let Ok(parsed_exp) = serde_json::from_str::<serde_json::Value>(exp_str) {
                json_matches(&parsed_exp, actual)
            } else {
                // Not JSON, compare as-is
                expected == actual
            }
        }
        (expected, serde_json::Value::String(act_str)) => {
            // Try to parse the actual string as JSON
            if let Ok(parsed_act) = serde_json::from_str::<serde_json::Value>(act_str) {
                json_matches(expected, &parsed_act)
            } else {
                // Not JSON, compare as-is
                expected == actual
            }
        }
        _ => expected == actual,
    }
}

// ============================================================================
// Curl Command Generation
// ============================================================================

fn generate_curl_command(port: u16, test_case: &TestCase) -> String {
    let mut cmd = format!("curl -X {} ", test_case.method);

    // Add headers
    for (name, value) in &test_case.headers {
        cmd.push_str(&format!("-H '{name}: {value}' "));
    }

    // Add body
    if let Some(ref body) = test_case.body {
        let escaped = body.replace('\'', "'\\''");
        cmd.push_str(&format!("-d '{escaped}' "));
    }

    // Build URL with query params
    let mut url = format!("'http://localhost:{}{}", port, test_case.path);
    if !test_case.query_params.is_empty() {
        let query_string: Vec<String> = test_case
            .query_params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        url = format!("{}?{}", url, query_string.join("&"));
    }
    url.push('\'');

    cmd.push_str(&url);
    cmd
}

// ============================================================================
// Summary Report
// ============================================================================

fn print_summary(summary: &VerificationSummary, show_curl: bool) {
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("{BOLD}Verification Summary{RESET}");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Imposters:  {}", summary.total_imposters);
    println!("  Stubs:      {}", summary.total_stubs);
    println!("  Tests:      {}", summary.total_tests);
    println!();
    println!("  {}Passed:  {}{}", GREEN, summary.passed, RESET);
    println!("  {}Failed:  {}{}", RED, summary.failed, RESET);
    println!("  {}Skipped: {}{}", YELLOW, summary.skipped, RESET);
    println!();

    if !summary.failures.is_empty() {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("{RED}Failure Details{RESET}");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        for (i, failure) in summary.failures.iter().enumerate() {
            println!();
            println!(
                "{}. Imposter :{} {} - Stub #{}{}",
                i + 1,
                failure.imposter_port,
                failure
                    .imposter_name
                    .as_ref()
                    .map(|n| format!("({n})"))
                    .unwrap_or_default(),
                failure.stub_index,
                failure
                    .stub_id
                    .as_ref()
                    .map(|id| format!(" [{id}]"))
                    .unwrap_or_default()
            );
            println!("   Request:  {}", failure.test_description);
            println!("   Expected: {}", failure.expected);
            println!("   {}Actual:   {}{}", RED, failure.actual, RESET);

            if show_curl {
                if let Some(ref curl) = failure.curl_command {
                    println!("   Curl:     {curl}");
                }
            }

            // Print failure reasons with hints
            if !failure.failure_reasons.is_empty() {
                println!();
                println!("   {BOLD}Why it failed:{RESET}");
                for reason in &failure.failure_reasons {
                    print_failure_reason(reason);
                }
            }
        }
        println!();
    }

    // Final status
    if summary.failed == 0 {
        println!("{GREEN}All tests passed!{RESET}");
    } else {
        println!(
            "{}{} test(s) failed. See details above.{}",
            RED, summary.failed, RESET
        );
    }
}

/// Print a single failure reason with hint and optional diff
fn print_failure_reason(reason: &FailureReason) {
    match reason {
        FailureReason::StatusMismatch { expected, actual } => {
            println!("   - {YELLOW}Status mismatch:{RESET} expected {GREEN}{expected}{RESET}, got {RED}{actual}{RESET}");
            println!("     {DIM}{}{RESET}", reason.hint());
        }
        FailureReason::HeaderMissing { header_name } => {
            println!("   - {YELLOW}Missing header:{RESET} '{header_name}'");
            println!("     {DIM}{}{RESET}", reason.hint());
        }
        FailureReason::HeaderMismatch {
            header_name,
            expected,
            actual,
        } => {
            println!("   - {YELLOW}Header mismatch:{RESET} '{header_name}'");
            println!("     Expected: {GREEN}\"{expected}\"{RESET}");
            println!("     Actual:   {RED}\"{actual}\"{RESET}");
        }
        FailureReason::BodyMissing { expected } => {
            println!("   - {YELLOW}Missing body:{RESET} expected response body but got none");
            println!("     {DIM}{}{RESET}", reason.hint());
            println!("     Expected body:");
            for line in expected.lines().take(10) {
                println!("       {GREEN}{line}{RESET}");
            }
            if expected.lines().count() > 10 {
                println!(
                    "       {DIM}... ({} more lines){RESET}",
                    expected.lines().count() - 10
                );
            }
        }
        FailureReason::BodyMismatch { expected, actual } => {
            println!("   - {YELLOW}Body mismatch:{RESET}");
            println!("     {DIM}{}{RESET}", reason.hint());
            print_diff(expected, actual);
        }
        FailureReason::RequestError(err) => {
            println!("   - {YELLOW}Request error:{RESET} {err}");
            println!("     {DIM}{}{RESET}", reason.hint());
        }
        FailureReason::TransportResetExpected { actual } => {
            println!("   - {YELLOW}Fault not triggered:{RESET} expected connection reset, got HTTP {actual}");
            println!("     {DIM}{}{RESET}", reason.hint());
        }
    }
}

/// Print a unified diff between expected and actual content
fn print_diff(expected: &str, actual: &str) {
    println!("     {DIM}Diff ({GREEN}-expected{DIM}, {RED}+actual{DIM}):{RESET}");

    let diff = TextDiff::from_lines(expected, actual);

    for change in diff.iter_all_changes() {
        let (sign, color) = match change.tag() {
            ChangeTag::Delete => ("-", GREEN),
            ChangeTag::Insert => ("+", RED),
            ChangeTag::Equal => (" ", RESET),
        };

        // Only show context and changes, skip too many equal lines
        if change.tag() == ChangeTag::Equal {
            print!(
                "     {DIM}{sign} {}{RESET}",
                change.value().trim_end_matches('\n')
            );
        } else {
            print!(
                "     {color}{sign} {}{RESET}",
                change.value().trim_end_matches('\n')
            );
        }
        println!();
    }
}

// ============================================================================
// Demo/Test Function for Enhanced Error Output
// ============================================================================

/// Demonstrates the enhanced error output by printing sample failure scenarios.
/// Run with: cargo run --bin rift-verify -- --demo
fn demo_enhanced_error_output() {
    println!("{BOLD}{CYAN}Enhanced Error Reporting Demo{RESET}");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    // Demo 1: Status Mismatch
    println!("{BOLD}1. Status Code Mismatch:{RESET}");
    let status_fail = FailureReason::StatusMismatch {
        expected: 200,
        actual: 404,
    };
    print_failure_reason(&status_fail);
    println!();

    // Demo 2: Header Missing
    println!("{BOLD}2. Missing Header:{RESET}");
    let header_missing = FailureReason::HeaderMissing {
        header_name: "X-Request-Id".to_string(),
    };
    print_failure_reason(&header_missing);
    println!();

    // Demo 3: Header Mismatch
    println!("{BOLD}3. Header Value Mismatch:{RESET}");
    let header_mismatch = FailureReason::HeaderMismatch {
        header_name: "Content-Type".to_string(),
        expected: "application/json".to_string(),
        actual: "text/plain".to_string(),
    };
    print_failure_reason(&header_mismatch);
    println!();

    // Demo 4: Body Mismatch with Diff
    println!("{BOLD}4. JSON Body Mismatch (with diff):{RESET}");
    let expected_json = r#"{
  "users": [
    {"id": 1, "name": "Alice"},
    {"id": 2, "name": "Bob"}
  ],
  "total": 2
}"#;
    let actual_json = r#"{
  "users": [
    {"id": 1, "name": "Alice"},
    {"id": 3, "name": "Charlie"}
  ],
  "total": 2,
  "extra": "unexpected"
}"#;
    let body_mismatch = FailureReason::BodyMismatch {
        expected: expected_json.to_string(),
        actual: actual_json.to_string(),
    };
    print_failure_reason(&body_mismatch);
    println!();

    // Demo 5: Connection Error
    println!("{BOLD}5. Connection Error:{RESET}");
    let conn_error = FailureReason::RequestError("Connection refused (os error 61)".to_string());
    print_failure_reason(&conn_error);
    println!();

    // Demo 6: Body Missing
    println!("{BOLD}6. Missing Response Body:{RESET}");
    let body_missing = FailureReason::BodyMissing {
        expected: r#"{"status": "ok"}"#.to_string(),
    };
    print_failure_reason(&body_missing);
    println!();

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("{GREEN}Demo complete!{RESET}");
}

#[cfg(test)]
mod verify_tests {
    use super::*;
    use serde_json::json;

    // ── #1: not predicate ──────────────────────────────────────────────────
    #[test]
    fn not_predicate_generates_non_matching_path() {
        let preds = vec![json!({ "not": { "equals": { "path": "/secret" } } })];
        let (_method, path, _h, _q, _b) = parse_predicates(&preds);
        assert_ne!(
            path, "/secret",
            "generated path must NOT satisfy the inner equals"
        );
    }

    #[test]
    fn not_predicate_flips_method() {
        let preds = vec![json!({ "not": { "equals": { "method": "POST" } } })];
        let (method, _p, _h, _q, _b) = parse_predicates(&preds);
        assert_ne!(
            method, "POST",
            "generated method must NOT satisfy the inner equals"
        );
    }

    #[test]
    fn not_predicate_steers_away_from_default_method_and_path() {
        // The forbidden value equals the parser default (GET / "/"), so a naive `!= default`
        // guard would leave the request satisfying the inner predicate — it must still be steered.
        let (method, _p, _h, _q, _b) =
            parse_predicates(&[json!({ "not": { "equals": { "method": "GET" } } })]);
        assert_ne!(method, "GET");
        let (_m, path, _h, _q, _b) =
            parse_predicates(&[json!({ "not": { "equals": { "path": "/" } } })]);
        assert_ne!(path, "/");
    }

    // ── #1: xpath predicate ────────────────────────────────────────────────
    #[test]
    fn build_xml_from_xpath_nests_elements() {
        assert_eq!(
            build_xml_from_xpath("/order/id", "728839"),
            "<order><id>728839</id></order>"
        );
    }

    #[test]
    fn build_xml_from_xpath_single_element() {
        assert_eq!(build_xml_from_xpath("/root", "x"), "<root>x</root>");
    }

    #[test]
    fn xpath_builds_matching_xml_body() {
        let preds = vec![json!({
            "xpath": { "selector": "/order/id" },
            "equals": { "body": "728839" }
        })];
        let (_m, _p, _h, _q, body) = parse_predicates(&preds);
        assert_eq!(body.as_deref(), Some("<order><id>728839</id></order>"));
    }

    // ── #3: tcp fault detection + classification ───────────────────────────
    #[test]
    fn expects_tcp_fault_detects_rift_fault_tcp() {
        let responses = vec![json!({ "_rift": { "fault": { "tcp": "reset" } } })];
        assert!(expects_tcp_fault(&responses));
    }

    #[test]
    fn expects_tcp_fault_false_for_normal_and_latency() {
        assert!(!expects_tcp_fault(&[
            json!({ "is": { "statusCode": 200 } })
        ]));
        assert!(!expects_tcp_fault(&[
            json!({ "_rift": { "fault": { "latency": 50 } } })
        ]));
    }

    #[test]
    fn is_transport_reset_error_matches_connection_errors() {
        for msg in [
            "error sending request: connection reset by peer",
            "connection closed before message completed",
            "incomplete message",
            "tcp connect error: Connection reset",
            "connection aborted by peer",
            "broken pipe (os error 32)",
        ] {
            assert!(is_transport_reset_error(msg), "should match: {msg}");
        }
    }

    #[test]
    fn is_expected_reset_truth_table() {
        // Only a fault-expecting stub AND a transport reset counts as the expected outcome.
        assert!(is_expected_reset(true, "connection reset by peer"));
        assert!(!is_expected_reset(false, "connection reset by peer"));
        assert!(!is_expected_reset(true, "connection refused (os error 61)"));
        assert!(!is_expected_reset(false, "200 OK"));
    }

    #[test]
    fn is_transport_reset_error_ignores_status_like_errors() {
        assert!(!is_transport_reset_error("builder error: invalid URL"));
    }

    // ── #4a/#4b: URL construction ──────────────────────────────────────────
    #[test]
    fn build_target_url_direct_http() {
        assert_eq!(
            build_target_url("http://localhost:2525", false, "http", 4511, "/api/data"),
            "http://localhost:4511/api/data"
        );
    }

    #[test]
    fn build_target_url_https() {
        assert_eq!(
            build_target_url("http://localhost:2525", false, "https", 4545, "/secure"),
            "https://localhost:4545/secure"
        );
    }

    #[test]
    fn build_target_url_gateway() {
        assert_eq!(
            build_target_url("http://localhost:2525", true, "http", 4511, "/api/data"),
            "http://localhost:2525/__rift/4511/api/data"
        );
    }

    #[test]
    fn build_target_url_gateway_trims_trailing_slash() {
        assert_eq!(
            build_target_url("http://localhost:2525/", true, "http", 9, "/x"),
            "http://localhost:2525/__rift/9/x"
        );
    }

    // ── #4c: correlated isolation header ───────────────────────────────────
    #[test]
    fn flow_id_header_name_extracts_header_source() {
        assert_eq!(
            flow_id_header_name("header:X-Mock-Space").as_deref(),
            Some("X-Mock-Space")
        );
    }

    #[test]
    fn flow_id_header_name_none_for_port_source() {
        assert_eq!(flow_id_header_name("imposter_port"), None);
    }

    #[test]
    fn imposter_flow_header_navigates_flow_state() {
        let with_header: ImposterDetails = serde_json::from_value(json!({
            "port": 4500, "protocol": "http", "stubs": [],
            "flowState": { "mountebankStateMapping": { "flowIdSource": "header:X-Mock-Space" } }
        }))
        .unwrap();
        assert_eq!(with_header.flow_header().as_deref(), Some("X-Mock-Space"));

        let port_source: ImposterDetails = serde_json::from_value(json!({
            "port": 4500, "protocol": "http", "stubs": [],
            "flowState": { "mountebankStateMapping": { "flowIdSource": "imposter_port" } }
        }))
        .unwrap();
        assert_eq!(port_source.flow_header(), None);

        let no_flow_state: ImposterDetails = serde_json::from_value(json!({
            "port": 4500, "protocol": "http", "stubs": []
        }))
        .unwrap();
        assert_eq!(no_flow_state.flow_header(), None);
    }
}
