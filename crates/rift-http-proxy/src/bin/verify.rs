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
use rift_http_proxy::stub_requests::{Stub, TestCase, check_if_dynamic, generate_test_cases};
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::OnceLock;
use std::time::Duration;

#[path = "verify/dynamic.rs"]
mod dynamic;

/// Runtime ANSI color codes. Resolved once in `main` from `NO_COLOR`/TTY/json-mode via
/// `Palette::detect`, then read anywhere via `palette()`. Fields are empty strings when color is
/// disabled, so `{green}`-style interpolation becomes a no-op instead of requiring call-site branching.
#[derive(Debug, Clone, Copy)]
struct Palette {
    green: &'static str,
    red: &'static str,
    yellow: &'static str,
    cyan: &'static str,
    bold: &'static str,
    dim: &'static str,
    reset: &'static str,
}

impl Palette {
    const PLAIN: Palette = Palette {
        green: "",
        red: "",
        yellow: "",
        cyan: "",
        bold: "",
        dim: "",
        reset: "",
    };

    /// Color is on only for an interactive text-mode session: never in `-o json` (stdout must be
    /// pure JSON), never with `NO_COLOR` set, and never when stdout is piped/redirected.
    fn detect(json_mode: bool) -> Self {
        let color =
            !json_mode && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        if color {
            Palette {
                green: "\x1b[32m",
                red: "\x1b[31m",
                yellow: "\x1b[33m",
                cyan: "\x1b[36m",
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                reset: "\x1b[0m",
            }
        } else {
            Palette::PLAIN
        }
    }
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// The process-wide palette, set once in `main` before anything is printed. Falls back to plain
/// (no color) rather than panicking if read before `main` initializes it.
fn palette() -> Palette {
    PALETTE.get().copied().unwrap_or(Palette::PLAIN)
}

/// True once `main` has committed to json output mode. Printing helpers consult this so decoration
/// (banners, progress, the human summary) never lands on stdout alongside the JSON payload.
static JSON_MODE: OnceLock<bool> = OnceLock::new();

fn json_mode() -> bool {
    JSON_MODE.get().copied().unwrap_or(false)
}

/// Print to stdout normally, or to stderr when running in `-o json` — stdout in json mode is
/// reserved exclusively for the final serialized summary.
macro_rules! outln {
    ($($arg:tt)*) => {{
        if json_mode() {
            eprintln!($($arg)*);
        } else {
            println!($($arg)*);
        }
    }};
}

/// Same routing as `outln!`, without a trailing newline (mirrors `print!`).
macro_rules! outw {
    ($($arg:tt)*) => {{
        if json_mode() {
            eprint!($($arg)*);
        } else {
            print!($($arg)*);
        }
    }};
}

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

    /// Report stubs with dynamic responses (inject/proxy/script/cycling/faults) as skipped instead
    /// of requesting them. Off by default: a dynamic stub is requested and passes on any 2xx status.
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

    /// Opt-in: assert dynamic behaviors properly (issue #251), not just their status. Stands up an
    /// embedded mock upstream for `proxy`, runs any `_verify` sequence against a fresh imposter,
    /// and asserts deterministic `_rift.fault` outcomes. Off by default.
    #[arg(long)]
    verify_dynamic: bool,

    /// Fallback correlated-isolation header name (issue #260): used only when the imposter's
    /// `flowIdSource` isn't discoverable from `GET /imposters`. A detected per-imposter header takes
    /// precedence, so this never clobbers a correctly-detected, differently-named imposter.
    #[arg(long)]
    flow_id_header: Option<String>,

    /// Output format: text (default), json. In json mode, stdout carries only the final summary
    /// (issue #347) — all human-readable progress and the decorative banner go to stderr instead.
    #[arg(short = 'o', long, default_value = "text")]
    output: String,
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
    /// Raw `_rift` extensions block. `GET /imposters` exposes the flow-state config under
    /// `_rift.flowState` (issue #260); parsed loosely — we only need `flowIdSource`.
    #[serde(default, rename = "_rift")]
    rift: Option<serde_json::Value>,
}

impl ImposterDetails {
    /// The header name carrying the correlation/space id, when this imposter isolates flows by a
    /// request header (`flowIdSource: "header:<Name>"`). `None` for the default `imposter_port`.
    fn flow_header(&self) -> Option<String> {
        self.rift
            .as_ref()?
            .get("flowState")?
            .get("flowIdSource")?
            .as_str()
            .and_then(flow_id_header_name)
    }
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
            FailureReason::StatusMismatch { expected, actual } => match *actual {
                404 => format!(
                    "Hint: Got 404 instead of {expected}. The stub predicate may not match the test request path/method."
                ),
                500 => {
                    format!("Hint: Got 500 instead of {expected}. Check server logs for errors.")
                }
                _ => format!(
                    "Hint: Expected status {expected} but got {actual}. Verify the stub response configuration."
                ),
            },
            FailureReason::HeaderMissing { header_name } => {
                format!(
                    "Hint: Expected header '{header_name}' is missing from the response. Add it to the stub's response headers."
                )
            }
            FailureReason::HeaderMismatch {
                header_name,
                expected,
                actual,
            } => {
                format!(
                    "Hint: Header '{header_name}' has wrong value.\n       Expected: \"{expected}\"\n       Actual:   \"{actual}\""
                )
            }
            FailureReason::BodyMismatch { .. } => {
                "Hint: Response body doesn't match. See diff below for details.".to_string()
            }
            FailureReason::BodyMissing { .. } => {
                "Hint: Expected a response body but got an empty response.".to_string()
            }
            FailureReason::TransportResetExpected { actual } => {
                format!(
                    "Hint: This stub injects a _rift.fault.tcp reset, so the connection should drop; instead it answered HTTP {actual}. The fault did not fire."
                )
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
    let json = args.output == "json";
    let _ = JSON_MODE.set(json);
    let _ = PALETTE.set(Palette::detect(json));

    let client = build_verify_client(args.timeout, args.insecure)?;

    // Check if demo mode
    if args.demo {
        demo_enhanced_error_output();
        return Ok(());
    }

    let Palette {
        green,
        red,
        yellow,
        cyan,
        bold,
        dim,
        reset,
    } = palette();

    // Banner and per-test progress are decoration, not data: `outln!` keeps them off stdout
    // entirely in json mode, so stdout carries only the final serialized summary.
    outln!("{bold}{cyan}Rift Stub Verifier{reset}");
    outln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    outln!("Admin URL: {}", args.admin_url);
    outln!();

    // Fetch imposters
    let FetchedImposters {
        imposters,
        unfetched,
    } = fetch_imposters(&client, &args.admin_url, args.port).await?;

    if imposters.is_empty() && unfetched.is_empty() {
        outln!("{yellow}Warning:{reset} No imposters found");
        if json {
            let payload = serde_json::json!({
                "imposters": 0, "stubs": 0, "tests": 0, "passed": 0, "failed": 0, "skipped": 0,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        }
        return Ok(());
    }

    let mut summary = VerificationSummary {
        total_imposters: imposters.len() + unfetched.len(),
        ..Default::default()
    };

    // A listed imposter whose detail could not be fetched was never verified: a FAILURE, visible
    // in the exit code, not a silent omission from the run.
    for (port, detail) in unfetched {
        outln!("{red}FAIL{reset} imposter {port} — {detail}");
        outln!();
        summary.failed += 1;
        summary.failures.push(FailureDetails {
            imposter_port: port,
            imposter_name: None,
            stub_index: 0,
            stub_id: None,
            test_description: "imposter fetch".to_string(),
            expected: "imposter detail fetched for verification".to_string(),
            actual: detail,
            curl_command: None,
            failure_reasons: vec![],
        });
    }

    // Process each imposter
    for imposter in &imposters {
        outln!(
            "{}Imposter:{} {} (port {})",
            bold,
            reset,
            imposter.name.as_deref().unwrap_or("unnamed"),
            imposter.port
        );

        summary.total_stubs += imposter.stubs.len();

        if imposter.stubs.is_empty() {
            outln!("   └─ No stubs defined");
            outln!();
            continue;
        }

        let flow_header =
            resolve_flow_header(imposter.flow_header(), args.flow_id_header.as_deref());
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
                    outln!("   {dim}{curl}{reset}");
                }

                if let Some(reason) = &test_case.skip_reason {
                    // No-match stubs count as passed (they pass by design)
                    // Other skipped stubs (dynamic, etc.) count as skipped
                    if test_case.is_no_match_stub {
                        summary.passed += 1;
                        if args.verbose {
                            outln!(
                                "   {}PASS{} Stub #{} - {} {} ({})",
                                green,
                                reset,
                                stub_index,
                                test_case.method,
                                test_case.path,
                                reason
                            );
                        }
                    } else {
                        summary.skipped += 1;
                        if args.verbose {
                            outln!(
                                "   {}SKIP{} Stub #{} - {}",
                                yellow,
                                reset,
                                stub_index,
                                test_case.skip_reason.as_ref().unwrap()
                            );
                        }
                    }
                    continue;
                }

                if args.dry_run {
                    outln!(
                        "   {}DRY-RUN{} Stub #{}{} - {} {}",
                        cyan,
                        reset,
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
                        outln!(
                            "   {}PASS{} Stub #{}{} - {} {} -> {} ({}ms)",
                            green,
                            reset,
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

                    outln!(
                        "   {}FAIL{} Stub #{}{} - {} {}",
                        red,
                        reset,
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
                        outln!("   {bold}Why it failed:{reset}");
                        for reason in &failure.failure_reasons {
                            print_failure_reason(reason);
                        }
                    }

                    summary.failures.push(failure);
                }
            }
        }
        outln!();
    }

    // Opt-in dynamic-behavior assertion (issue #251): proxy / `_verify` sequences / faults.
    if args.verify_dynamic {
        run_dynamic_verification(&client, &args.admin_url, &imposters, &mut summary).await;
    }

    // Print summary
    print_summary(&summary, args.show_curl);

    // json mode: stdout carries only this single, pure-JSON summary payload (issue #347); every
    // other message above went to stderr via `outln!`.
    if json {
        let payload = serde_json::json!({
            "imposters": summary.total_imposters,
            "stubs": summary.total_stubs,
            "tests": summary.total_tests,
            "passed": summary.passed,
            "failed": summary.failed,
            "skipped": summary.skipped,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    }

    // Exit with error code if any failures
    if summary.failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Run the opt-in dynamic-behavior assertions (issue #251) for every imposter and fold the
/// resulting checks into the summary. Operates on the raw `GET /imposters/:port?replayable=true`
/// JSON: all engine-preserved stub fields (`_verify`, `proxy`, `_rift.fault`) are visible, and a
/// `_verify` recreate carries the whole config — `defaultResponse`, `defaultForward`, `_rift`,
/// TLS — which the plain detail does not print in full (issue #1334).
async fn run_dynamic_verification(
    client: &Client,
    admin_url: &str,
    imposters: &[ImposterDetails],
    summary: &mut VerificationSummary,
) {
    let Palette {
        green,
        red,
        yellow,
        cyan,
        bold,
        reset,
        ..
    } = palette();
    let verifier = dynamic::DynamicVerifier { client, admin_url };
    outln!();
    outln!("{bold}{cyan}Dynamic assertions (--verify-dynamic){reset}");

    for imposter in imposters {
        // The imposter list was already fetched successfully, so a per-imposter GET/parse failure
        // here is anomalous — its dynamic checks could not run. Count it as a FAILURE (visible in
        // the exit code) rather than a silent skip that still exits 0.
        let fetch = client
            .get(format!(
                "{admin_url}/imposters/{}?replayable=true",
                imposter.port
            ))
            .send()
            .await
            .map_err(|e| format!("fetch: {e}"))
            .and_then(|resp| {
                // A 4xx/5xx error body is JSON too; parsed as an imposter it has no stubs, so the
                // imposter's dynamic checks would vanish instead of failing.
                if resp.status().is_success() {
                    Ok(resp)
                } else {
                    Err(format!("fetch: HTTP {}", resp.status()))
                }
            });
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
                outln!("   {yellow}SKIP{reset} {} — {}", check.label, check.detail);
            } else if check.passed {
                summary.passed += 1;
                outln!("   {green}PASS{reset} {}", check.label);
            } else {
                summary.failed += 1;
                outln!("   {red}FAIL{reset} {} — {}", check.label, check.detail);
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
    let Palette { red, reset, .. } = palette();
    outln!(
        "   {red}FAIL{reset} imposter {} dynamic fetch — {detail}",
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

/// The listed imposters whose details were fetched, and the `(port, why)` of each that was listed
/// but whose detail answered an HTTP error.
struct FetchedImposters {
    imposters: Vec<ImposterDetails>,
    unfetched: Vec<(u16, String)>,
}

async fn fetch_imposters(
    client: &Client,
    admin_url: &str,
    filter_port: Option<u16>,
) -> Result<FetchedImposters, Box<dyn std::error::Error>> {
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
    let mut unfetched = Vec::new();

    for link in imposter_links {
        if let Some(port) = filter_port
            && link.port != port
        {
            continue;
        }

        // Fetch full imposter details
        let detail_url = format!("{}/imposters/{}", admin_url, link.port);
        let detail_response = client.get(&detail_url).send().await?;

        if detail_response.status().is_success() {
            let details: ImposterDetails = detail_response.json().await?;
            imposters.push(details);
        } else {
            unfetched.push((
                link.port,
                format!("fetch {detail_url}: HTTP {}", detail_response.status()),
            ));
        }
    }

    Ok(FetchedImposters {
        imposters,
        unfetched,
    })
}

/// Render an error together with its full `source()` chain. reqwest's top-level `Display` is only
/// `error sending request for url (...)`; the actual cause (e.g. "connection reset by peer") lives
/// in the source chain, so classifying on `to_string()` alone misses transport resets (issue #258).
fn error_chain_string(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// Classify a request error string as a connection-level reset/abort (the signature of a TCP
/// fault) rather than an application error. Used to PASS `_rift.fault.tcp` stubs (finding #3).
/// Pass the full chain from `error_chain_string` — the reset cause is not in reqwest's top Display.
fn is_transport_reset_error(err: &str) -> bool {
    let e = err.to_lowercase();
    // `connection closed`/`incomplete message` can't distinguish an intentional reset from an
    // imposter crash mid-response; the dynamic path's control-request health check guards that.
    e.contains("connection reset")
        || e.contains("connection closed")
        || e.contains("connection aborted")
        || e.contains("broken pipe")
        || e.contains("incomplete message")
        // `RANDOM_DATA_THEN_CLOSE` (issue #273) writes garbage then closes, so the client fails to
        // parse the bytes as HTTP. The control-request health check upstream guards against a sick
        // imposter being mistaken for this fault.
        || e.contains("invalid http version")
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

/// Resolve the correlated-isolation header name for an imposter (issue #260): prefer what the
/// imposter exposes; fall back to the `--flow-id-header` override only when detection is
/// unavailable, so a global override never clobbers a correctly-detected, differently-named imposter.
fn resolve_flow_header(detected: Option<String>, fallback: Option<&str>) -> Option<String> {
    detected.or_else(|| fallback.map(str::to_string))
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
    // Any method a stub predicates on, not a fixed list with a GET fallback (issue #1380). The
    // predicate matches case-insensitively by default, so `options` in a stub means OPTIONS.
    let method = match reqwest::Method::from_bytes(test_case.method.to_uppercase().as_bytes()) {
        Ok(method) => method,
        Err(e) => {
            let error_msg = format!("invalid request method `{}`: {e}", test_case.method);
            return TestResult {
                test_case: test_case.clone(),
                success: false,
                actual_status: None,
                actual_headers: None,
                actual_body: None,
                error: Some(error_msg.clone()),
                duration_ms: start.elapsed().as_millis(),
                failure_reasons: vec![FailureReason::RequestError(error_msg)],
            };
        }
    };
    let mut request = client.request(method, &url);

    // Add headers
    for (name, value) in &test_case.headers {
        request = request.header(name, value);
    }

    // Correlated-isolation header (issue #223): route this request to the stub's own space when it
    // declares one (issue #260), else the global `--space`.
    if let Some(flow_header) = &test_case.flow_header {
        let value = test_case.flow_space.as_deref().unwrap_or(space);
        request = request.header(flow_header, value);
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
            let error_msg = error_chain_string(&e);
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
                    // A template body is expanded by the engine; the literal can't be asserted.
                    if !contains_template(&expected_plain) && actual_text != &expected_plain {
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

/// True when a string carries a Rift `{{...}}` template (`{{NOW}}`, `{{DAYS+N}}`, `{{MONTHS+N}}`,
/// #245). The engine expands these, so the literal source value cannot be asserted (issue #259).
/// Requires a `{{` followed by a later `}}` so stray/reversed braces (`}} x {{`) don't suppress a
/// real body assertion.
fn contains_template(s: &str) -> bool {
    s.find("{{")
        .is_some_and(|open| s[open + 2..].contains("}}"))
}

/// Checks if two JSON values are semantically equal.
/// This handles:
/// - Different key ordering in objects
/// - Compact vs pretty-printed formatting
/// - String values that contain JSON (parses and compares them)
/// - `{{...}}` template strings on the expected side (wildcards — the engine expands them)
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
        // A `{{...}}` Rift template (e.g. `{{NOW}}`, `{{DAYS+N}}`, #245/#259) is expanded by the
        // engine, so the literal source value can't be asserted — treat it as a wildcard. Sibling
        // (non-template) fields are still compared by the surrounding object/array recursion.
        (serde_json::Value::String(exp_str), _) if contains_template(exp_str) => true,
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
    let Palette {
        green,
        red,
        yellow,
        bold,
        reset,
        ..
    } = palette();

    outln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    outln!("{bold}Verification Summary{reset}");
    outln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    outln!("  Imposters:  {}", summary.total_imposters);
    outln!("  Stubs:      {}", summary.total_stubs);
    outln!("  Tests:      {}", summary.total_tests);
    outln!();
    outln!("  {}Passed:  {}{}", green, summary.passed, reset);
    outln!("  {}Failed:  {}{}", red, summary.failed, reset);
    outln!("  {}Skipped: {}{}", yellow, summary.skipped, reset);
    outln!();

    if !summary.failures.is_empty() {
        outln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        outln!("{red}Failure Details{reset}");
        outln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        for (i, failure) in summary.failures.iter().enumerate() {
            outln!();
            outln!(
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
            outln!("   Request:  {}", failure.test_description);
            outln!("   Expected: {}", failure.expected);
            outln!("   {}Actual:   {}{}", red, failure.actual, reset);

            if show_curl && let Some(ref curl) = failure.curl_command {
                outln!("   Curl:     {curl}");
            }

            // Print failure reasons with hints
            if !failure.failure_reasons.is_empty() {
                outln!();
                outln!("   {bold}Why it failed:{reset}");
                for reason in &failure.failure_reasons {
                    print_failure_reason(reason);
                }
            }
        }
        outln!();
    }

    // Final status
    if summary.failed == 0 {
        outln!("{green}All tests passed!{reset}");
    } else {
        outln!(
            "{}{} test(s) failed. See details above.{}",
            red,
            summary.failed,
            reset
        );
    }
}

/// Print a single failure reason with hint and optional diff
fn print_failure_reason(reason: &FailureReason) {
    let Palette {
        green,
        red,
        yellow,
        dim,
        reset,
        ..
    } = palette();

    match reason {
        FailureReason::StatusMismatch { expected, actual } => {
            outln!(
                "   - {yellow}Status mismatch:{reset} expected {green}{expected}{reset}, got {red}{actual}{reset}"
            );
            outln!("     {dim}{}{reset}", reason.hint());
        }
        FailureReason::HeaderMissing { header_name } => {
            outln!("   - {yellow}Missing header:{reset} '{header_name}'");
            outln!("     {dim}{}{reset}", reason.hint());
        }
        FailureReason::HeaderMismatch {
            header_name,
            expected,
            actual,
        } => {
            outln!("   - {yellow}Header mismatch:{reset} '{header_name}'");
            outln!("     Expected: {green}\"{expected}\"{reset}");
            outln!("     Actual:   {red}\"{actual}\"{reset}");
        }
        FailureReason::BodyMissing { expected } => {
            outln!("   - {yellow}Missing body:{reset} expected response body but got none");
            outln!("     {dim}{}{reset}", reason.hint());
            outln!("     Expected body:");
            for line in expected.lines().take(10) {
                outln!("       {green}{line}{reset}");
            }
            if expected.lines().count() > 10 {
                outln!(
                    "       {dim}... ({} more lines){reset}",
                    expected.lines().count() - 10
                );
            }
        }
        FailureReason::BodyMismatch { expected, actual } => {
            outln!("   - {yellow}Body mismatch:{reset}");
            outln!("     {dim}{}{reset}", reason.hint());
            print_diff(expected, actual);
        }
        FailureReason::RequestError(err) => {
            outln!("   - {yellow}Request error:{reset} {err}");
            outln!("     {dim}{}{reset}", reason.hint());
        }
        FailureReason::TransportResetExpected { actual } => {
            outln!(
                "   - {yellow}Fault not triggered:{reset} expected connection reset, got HTTP {actual}"
            );
            outln!("     {dim}{}{reset}", reason.hint());
        }
    }
}

/// Print a unified diff between expected and actual content
fn print_diff(expected: &str, actual: &str) {
    let Palette {
        green,
        red,
        dim,
        reset,
        ..
    } = palette();

    outln!("     {dim}Diff ({green}-expected{dim}, {red}+actual{dim}):{reset}");

    let diff = TextDiff::from_lines(expected, actual);

    for change in diff.iter_all_changes() {
        let (sign, color) = match change.tag() {
            ChangeTag::Delete => ("-", green),
            ChangeTag::Insert => ("+", red),
            ChangeTag::Equal => (" ", reset),
        };

        // Only show context and changes, skip too many equal lines
        if change.tag() == ChangeTag::Equal {
            outw!(
                "     {dim}{sign} {}{reset}",
                change.value().trim_end_matches('\n')
            );
        } else {
            outw!(
                "     {color}{sign} {}{reset}",
                change.value().trim_end_matches('\n')
            );
        }
        outln!();
    }
}

// ============================================================================
// Demo/Test Function for Enhanced Error Output
// ============================================================================

/// Demonstrates the enhanced error output by printing sample failure scenarios.
/// Run with: cargo run --bin rift-verify -- --demo
fn demo_enhanced_error_output() {
    let Palette {
        green,
        cyan,
        bold,
        reset,
        ..
    } = palette();

    outln!("{bold}{cyan}Enhanced Error Reporting Demo{reset}");
    outln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    outln!();

    // Demo 1: Status Mismatch
    outln!("{bold}1. Status Code Mismatch:{reset}");
    let status_fail = FailureReason::StatusMismatch {
        expected: 200,
        actual: 404,
    };
    print_failure_reason(&status_fail);
    outln!();

    // Demo 2: Header Missing
    outln!("{bold}2. Missing Header:{reset}");
    let header_missing = FailureReason::HeaderMissing {
        header_name: "X-Request-Id".to_string(),
    };
    print_failure_reason(&header_missing);
    outln!();

    // Demo 3: Header Mismatch
    outln!("{bold}3. Header Value Mismatch:{reset}");
    let header_mismatch = FailureReason::HeaderMismatch {
        header_name: "Content-Type".to_string(),
        expected: "application/json".to_string(),
        actual: "text/plain".to_string(),
    };
    print_failure_reason(&header_mismatch);
    outln!();

    // Demo 4: Body Mismatch with Diff
    outln!("{bold}4. JSON Body Mismatch (with diff):{reset}");
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
    outln!();

    // Demo 5: Connection Error
    outln!("{bold}5. Connection Error:{reset}");
    let conn_error = FailureReason::RequestError("Connection refused (os error 61)".to_string());
    print_failure_reason(&conn_error);
    outln!();

    // Demo 6: Body Missing
    outln!("{bold}6. Missing Response Body:{reset}");
    let body_missing = FailureReason::BodyMissing {
        expected: r#"{"status": "ok"}"#.to_string(),
    };
    print_failure_reason(&body_missing);
    outln!();

    outln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    outln!("{green}Demo complete!{reset}");
}

/// The HTTP client every verification request goes through.
///
/// Idle-connection reuse is **off** (issue #982). `--verify-dynamic` drives a succession of
/// throwaway imposters, each created with `port: 0` and deleted straight after, so they land on
/// *recycled* ephemeral ports. A pooled socket to `127.0.0.1:<port>` therefore outlives the
/// imposter it was opened to: the next throwaway that happens to get the same port inherits a
/// connection whose peer is gone, and the first request on it dies with `Connection reset by peer`
/// — which then cascades, because the lost request is usually the one that would have advanced a
/// counter or scenario state.
///
/// Pooling across throwaway imposters is never valid, so there is nothing here to keep. Contrast
/// the imposter proxy client, where #482 turned pooling ON deliberately: that one talks to a single
/// stable upstream for the process lifetime.
fn build_verify_client(timeout_secs: u64, insecure: bool) -> Result<Client, reqwest::Error> {
    rift_http_proxy::http_client_builder()
        .timeout(Duration::from_secs(timeout_secs))
        .danger_accept_invalid_certs(insecure)
        .pool_max_idle_per_host(0)
        .build()
}

#[cfg(test)]
mod verify_tests {
    use super::*;

    /// Issue #1244: `--skip-dynamic` is an opt-in switch. Without it a dynamic stub is requested
    /// (and checked loosely); with it the stub is reported as a skip. The flag takes no value, so
    /// the documented `--skip-dynamic --verify-dynamic` invocation parses and a
    /// `--skip-dynamic=false` does not.
    #[test]
    fn skip_dynamic_is_an_opt_in_switch() {
        let args = Args::try_parse_from(["rift-verify"]).expect("no flags");
        assert!(!args.skip_dynamic, "dynamic stubs are requested by default");
        let args = Args::try_parse_from(["rift-verify", "--skip-dynamic", "--verify-dynamic"])
            .expect("the documented pair parses");
        assert!(args.skip_dynamic && args.verify_dynamic);
        assert!(Args::try_parse_from(["rift-verify", "--skip-dynamic=false"]).is_err());

        let stub: Stub = serde_json::from_value(serde_json::json!({
            "responses": [{"inject": "function () { return { statusCode: 418 }; }"}]
        }))
        .expect("stub");
        let requested = generate_test_cases(0, &stub, false, "http", None);
        assert_eq!(requested.len(), 1);
        assert!(requested[0].is_dynamic && requested[0].skip_reason.is_none());
        let skipped = generate_test_cases(0, &stub, true, "http", None);
        assert!(skipped[0].skip_reason.is_some());
    }

    /// Issue #1093: a null behavior key configures nothing, so it must not make a stub "dynamic".
    #[test]
    fn a_null_behavior_key_does_not_make_a_response_dynamic() {
        for responses in [
            vec![
                serde_json::json!({"is": {"statusCode": 200}, "_behaviors": {"copy": null, "repeat": null}}),
            ],
            vec![
                serde_json::json!({"is": {"statusCode": 200}, "behaviors": [{"shellTransform": null}]}),
            ],
        ] {
            assert_eq!(check_if_dynamic(&responses), (false, None), "{responses:?}");
        }
        let live = vec![
            serde_json::json!({"is": {"statusCode": 200}, "_behaviors": {"wait": null, "copy": [{"from": "path", "into": "${p}", "using": {"method": "regex", "selector": ".*"}}]}}),
        ];
        assert!(check_if_dynamic(&live).0, "a present copy is still dynamic");
    }

    /// Issue #1103: `copy`, `lookup` and `shellTransform` are lists in the engine, and an empty one
    /// runs nothing, so the response is static and must be asserted rather than skipped.
    #[test]
    fn an_empty_behavior_list_does_not_make_a_response_dynamic() {
        for block in [
            serde_json::json!({"copy": []}),
            serde_json::json!({"lookup": []}),
            serde_json::json!({"shellTransform": []}),
            serde_json::json!({"copy": [], "lookup": [], "shellTransform": [], "wait": 5}),
        ] {
            for response in [
                serde_json::json!({"is": {"statusCode": 200}, "_behaviors": block.clone()}),
                serde_json::json!({"is": {"statusCode": 200}, "behaviors": [block.clone()]}),
            ] {
                assert_eq!(
                    check_if_dynamic(std::slice::from_ref(&response)),
                    (false, None),
                    "{response}"
                );
            }
        }
    }

    #[test]
    fn a_configured_behavior_is_still_dynamic_with_its_label() {
        let cases = [
            (
                serde_json::json!({"repeat": 1}),
                "repeat behavior (stateful)",
            ),
            (
                serde_json::json!({"decorate": ""}),
                "decorate behavior (dynamic)",
            ),
            (
                serde_json::json!({"copy": {"from": "path", "into": "${p}", "using": {"method": "regex", "selector": ".*"}}}),
                "copy behavior (request-derived)",
            ),
            (
                serde_json::json!({"lookup": [{"key": {"from": "path", "using": {"method": "regex", "selector": ".*"}}, "fromDataSource": {"csv": {"path": "x.csv", "keyColumn": "k"}}, "into": "${row}"}]}),
                "lookup behavior (data-source)",
            ),
            (
                serde_json::json!({"shellTransform": "echo hi"}),
                "shellTransform behavior (external)",
            ),
            (
                serde_json::json!({"shellTransform": ["echo hi"]}),
                "shellTransform behavior (external)",
            ),
        ];
        for (block, label) in cases {
            for response in [
                serde_json::json!({"is": {"statusCode": 200}, "behaviors": [block.clone()]}),
                serde_json::json!({"is": {"statusCode": 200}, "_behaviors": block.clone()}),
            ] {
                assert_eq!(
                    check_if_dynamic(std::slice::from_ref(&response)),
                    (true, Some(label.to_string())),
                    "{response}"
                );
            }
        }
    }

    /// rift-verify reads `GET /imposters/:port`, where the engine has already chosen between
    /// `_behaviors` and `behaviors` and folded an array last-write-wins. This pins that verify's
    /// view of a response is the engine's own serialized selection, not the source file's.
    #[test]
    fn verify_reads_the_engines_folded_behaviors_selection() {
        // Decoded as an embedder replays a stored config, which normalises as a door does: a
        // `{"repeat": n}` element is refused at a door since #1582, but an older engine stored it.
        let wire = |stub: serde_json::Value| -> Vec<serde_json::Value> {
            let parsed: rift_mock_core::imposter::Stub =
                rift_mock_core::imposter::deserialize_replayed(stub)
                    .expect("stub parses in the engine");
            let serialized = serde_json::to_value(&parsed).expect("stub serializes");
            serialized["responses"]
                .as_array()
                .expect("responses array on the wire")
                .clone()
        };

        let later_null_clears_repeat = wire(serde_json::json!({"responses": [
            {"is": {"statusCode": 200}, "behaviors": [{"repeat": 2}, {"repeat": null}]}
        ]}));
        assert_eq!(check_if_dynamic(&later_null_clears_repeat), (false, None));

        // Issue #1709: beside `behaviors`, `_behaviors` is ignored, its `repeat` included.
        let behaviors_wins = wire(serde_json::json!({"responses": [
            {"is": {"statusCode": 200}, "_behaviors": {"repeat": 2}, "behaviors": [{"wait": 5}]}
        ]}));
        assert_eq!(check_if_dynamic(&behaviors_wins), (false, None));

        let empty_list_survives_the_round_trip = wire(serde_json::json!({"responses": [
            {"is": {"statusCode": 200}, "_behaviors": {"copy": []}}
        ]}));
        assert_eq!(
            check_if_dynamic(&empty_list_survives_the_round_trip),
            (false, None)
        );

        // Issue #1191: an engine writes `repeat` on the response, as Mountebank does.
        let top_level = |repeat: serde_json::Value| {
            check_if_dynamic(&[serde_json::json!({"is": {"statusCode": 200}, "repeat": repeat})])
        };
        assert_eq!(
            top_level(serde_json::json!(2)),
            (true, Some("repeat behavior (stateful)".to_string()))
        );
        assert_eq!(top_level(serde_json::Value::Null), (false, None));

        let object_form = wire(serde_json::json!({"responses": [
            {"is": {"statusCode": 200}, "behaviors": {"repeat": 2}}
        ]}));
        assert_eq!(
            check_if_dynamic(&object_form),
            (true, Some("repeat behavior (stateful)".to_string()))
        );
    }

    // Issue #982: the verify client must open a NEW connection per request.
    //
    // `--verify-dynamic` creates a throwaway imposter with `port: 0`, drives it, deletes it, and
    // repeats — so successive throwaways land on recycled ephemeral ports. A pooled idle socket
    // outlives the imposter it was opened to, and the next throwaway that lands on that port
    // inherits a dead connection whose first request fails with `Connection reset by peer`. That
    // then cascades: the lost request is usually the one that would have advanced a counter.
    //
    // Counting accepts is what makes this discriminating. With pooling on (reqwest's default, and
    // what shipped before this fix) the second request reuses the first socket and the server
    // accepts exactly once. Asserting on a response body would pass either way.
    #[tokio::test]
    async fn verify_client_does_not_reuse_idle_connections() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepts);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    // A REAL keep-alive server: serve request after request on the same socket
                    // and never close it. If this handled one request and dropped the socket, the
                    // client could not reuse the connection whatever its pool setting, and the
                    // assertion below would hold for the wrong reason.
                    let mut buf = [0u8; 1024];
                    loop {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                        if socket
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                            .await
                            .is_err()
                        {
                            return;
                        }
                        let _ = socket.flush().await;
                    }
                });
            }
        });

        let client = build_verify_client(5, false).expect("client builds");
        for _ in 0..2 {
            let resp = client
                .get(format!("http://127.0.0.1:{port}/"))
                .send()
                .await
                .expect("request succeeds");
            assert_eq!(resp.status(), 200);
            let _ = resp.text().await;
        }

        assert_eq!(
            accepts.load(Ordering::SeqCst),
            2,
            "each request must open its own connection; 1 accept means the idle socket was pooled \
             and reused, which is what breaks across recycled throwaway-imposter ports"
        );
    }
    use serde_json::json;

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

    #[test]
    fn error_chain_string_walks_source() {
        use std::fmt;
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "error sending request for url (http://127.0.0.1:1/x)")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let err = Outer(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer (os error 54)",
        ));
        let chain = error_chain_string(&err);
        // The top-level Display alone is NOT classifiable; the chained cause makes it so.
        assert!(!is_transport_reset_error(&err.to_string()));
        assert!(chain.contains("connection reset by peer"));
        assert!(is_transport_reset_error(&chain));
    }

    #[test]
    fn is_transport_reset_error_rejects_connection_refused_chain() {
        // A down imposter (connection refused) must NOT be mistaken for a reset (no false PASS).
        let refused = "error sending request for url (http://127.0.0.1:1/x): \
                       tcp connect error: connection refused (os error 61)";
        assert!(!is_transport_reset_error(refused));
    }

    // ── #273: dynamic-harness coverage gaps ─────────────────────────────────
    #[test]
    fn is_transport_reset_error_matches_random_data_garbage() {
        // RANDOM_DATA_THEN_CLOSE sends garbage then closes, so the client cannot parse HTTP.
        assert!(is_transport_reset_error(
            "error sending request for url (http://127.0.0.1:1/x): \
             client error (SendRequest): invalid HTTP version parsed"
        ));
    }

    // ── #259: date-template bodies are not asserted literally ───────────────
    #[test]
    fn contains_template_detects_rift_templates() {
        assert!(contains_template("{{NOW}}"));
        assert!(contains_template("expires {{DAYS+30}} from now"));
        assert!(contains_template("{{MONTHS+12}}"));
        assert!(!contains_template("2026-06-30T00:00:00+00:00"));
        assert!(!contains_template("plain body"));
        // Stray / reversed braces must NOT be treated as a template (no over-suppression).
        assert!(!contains_template("}} literal {{"));
        assert!(!contains_template("only {{ open"));
        assert!(!contains_template("only }} close"));
    }

    #[test]
    fn json_matches_template_in_array_element() {
        // The wildcard also applies inside arrays; a non-template element is still compared.
        assert!(json_matches(
            &serde_json::json!(["{{NOW}}", "fixed"]),
            &serde_json::json!(["2026-06-30T00:00:00+00:00", "fixed"])
        ));
        assert!(!json_matches(
            &serde_json::json!(["{{NOW}}", "fixed"]),
            &serde_json::json!(["2026-06-30T00:00:00+00:00", "WRONG"])
        ));
    }

    #[test]
    fn json_matches_template_string_is_wildcard() {
        // An expected `{{NOW}}` matches the engine's expanded timestamp.
        assert!(json_matches(
            &serde_json::json!("{{NOW}}"),
            &serde_json::json!("2026-06-30T16:06:39.878853+00:00")
        ));
    }

    #[test]
    fn json_matches_template_in_object_field() {
        let expected = serde_json::json!({
            "issued": "{{NOW}}", "expires": "{{DAYS+30}}", "kind": "token"
        });
        // Template fields are wildcards, but a non-template sibling is still compared.
        assert!(json_matches(
            &expected,
            &serde_json::json!({ "issued": "2026-06-30T00:00:00+00:00", "expires": "2026-07-30T00:00:00+00:00", "kind": "token" })
        ));
        assert!(!json_matches(
            &expected,
            &serde_json::json!({ "issued": "2026-06-30T00:00:00+00:00", "expires": "2026-07-30T00:00:00+00:00", "kind": "WRONG" })
        ));
    }

    #[test]
    fn json_matches_non_template_still_strict() {
        // Regression: a plain value mismatch must still fail.
        assert!(!json_matches(
            &serde_json::json!({ "a": "x" }),
            &serde_json::json!({ "a": "y" })
        ));
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
        // Flat shape (issue #266) — what `GET /imposters` emits.
        let with_header: ImposterDetails = serde_json::from_value(json!({
            "port": 4500, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "flowIdSource": "header:X-Mock-Space" } }
        }))
        .unwrap();
        assert_eq!(with_header.flow_header().as_deref(), Some("X-Mock-Space"));

        let port_source: ImposterDetails = serde_json::from_value(json!({
            "port": 4500, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "flowIdSource": "imposter_port" } }
        }))
        .unwrap();
        assert_eq!(port_source.flow_header(), None);

        let no_flow_state: ImposterDetails = serde_json::from_value(json!({
            "port": 4500, "protocol": "http", "stubs": []
        }))
        .unwrap();
        assert_eq!(no_flow_state.flow_header(), None);
    }

    #[test]
    fn resolve_flow_header_prefers_detection_then_override() {
        // Detection wins; the --flow-id-header override only fills the gap (no clobber).
        assert_eq!(
            resolve_flow_header(Some("X-Detected".to_string()), Some("X-Override")),
            Some("X-Detected".to_string())
        );
        assert_eq!(
            resolve_flow_header(None, Some("X-Override")),
            Some("X-Override".to_string())
        );
        assert_eq!(resolve_flow_header(None, None), None);
    }
}
