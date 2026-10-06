//! `rift intercept-ca generate|export` (issue #1274): a persistent intercept CA and its truststores,
//! made offline — before either the SUT or rift starts, which is when a container SUT needs them.

mod support;

use std::path::Path;
use std::process::{Command, Output};

fn rift(args: &[&str]) -> Output {
    Command::new(support::server_bin())
        .args(args)
        .output()
        .expect("run rift")
}

fn arg(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn generate(dir: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["intercept-ca", "generate", "--out-dir", arg(dir)];
    args.extend_from_slice(extra);
    rift(&args)
}

/// The JKS entry count and aliases, read from the store header.
fn jks_aliases(bytes: &[u8]) -> Vec<String> {
    let body = &bytes[..bytes.len() - 20];
    assert_eq!(&body[..4], &0xFEED_FEED_u32.to_be_bytes(), "JKS magic");
    let count = u32::from_be_bytes(body[8..12].try_into().unwrap());
    let mut p = 12;
    let mut aliases = Vec::new();
    for _ in 0..count {
        p += 4;
        let len = u16::from_be_bytes(body[p..p + 2].try_into().unwrap()) as usize;
        aliases.push(String::from_utf8(body[p + 2..p + 2 + len].to_vec()).unwrap());
        p += 2 + len + 8;
        let tlen = u16::from_be_bytes(body[p..p + 2].try_into().unwrap()) as usize;
        p += 2 + tlen;
        let clen = u32::from_be_bytes(body[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + clen;
    }
    aliases
}

#[tokio::test]
async fn generate_writes_a_pem_pair_the_listener_loads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = generate(dir.path(), &["--cn", "Acme CI CA", "--validity-days", "30"]);
    assert!(out.status.success(), "generate failed: {}", stderr(&out));

    let cert = std::fs::read_to_string(dir.path().join("ca-cert.pem")).expect("cert written");
    let key = std::fs::read_to_string(dir.path().join("ca-key.pem")).expect("key written");
    assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(key.contains("PRIVATE KEY"));
    let printed = stdout(&out);
    assert!(
        !printed.contains("PRIVATE KEY") && !stderr(&out).contains("PRIVATE KEY"),
        "the key is written to its file and never printed"
    );
    assert!(
        printed.contains("--intercept-ca-cert") && printed.contains("RIFT_INTERCEPT_CA_CERT_PEM"),
        "the output says how to launch with the pair: {printed}"
    );

    // The pair loads where rift's own listener loads a CA.
    use clap::Parser as _;
    let cli = rift_http_proxy::server::Cli::parse_from([
        "rift",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--local-only",
        "--intercept-port",
        "0",
        "--intercept-ca-cert",
        arg(&dir.path().join("ca-cert.pem")),
        "--intercept-ca-key",
        arg(&dir.path().join("ca-key.pem")),
    ]);
    let server = rift_http_proxy::server::ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("server starts with the generated CA");
    let served = reqwest::get(format!("http://{}/intercept/ca.pem", server.admin_addr()))
        .await
        .expect("ca.pem")
        .text()
        .await
        .expect("body");
    assert_eq!(
        served, cert,
        "the listener serves exactly the generated certificate"
    );
    server.shutdown().await;
}

#[test]
fn generate_refuses_to_overwrite_without_force() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(generate(dir.path(), &[]).status.success());
    let first = std::fs::read_to_string(dir.path().join("ca-cert.pem")).expect("cert");

    let again = generate(dir.path(), &[]);
    assert_eq!(again.status.code(), Some(2), "refusal exit code");
    assert!(
        stderr(&again).contains("--force"),
        "says how to overwrite: {}",
        stderr(&again)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ca-cert.pem")).expect("cert"),
        first,
        "a refused run leaves the existing pair alone"
    );

    let forced = generate(dir.path(), &["--force"]);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert_ne!(
        std::fs::read_to_string(dir.path().join("ca-cert.pem")).expect("cert"),
        first,
        "--force writes a new pair"
    );
}

/// A key file left over from a half-finished run must not be silently paired with a new cert.
#[test]
fn generate_refuses_when_only_the_key_exists() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("ca-key.pem"), "old").expect("plant key");
    let out = generate(dir.path(), &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!dir.path().join("ca-cert.pem").exists(), "nothing written");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ca-key.pem")).unwrap(),
        "old"
    );
}

#[cfg(unix)]
#[test]
fn generate_sets_key_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(generate(dir.path(), &[]).status.success());
    let mode = |name: &str| {
        std::fs::metadata(dir.path().join(name))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("ca-key.pem"), 0o600, "the private key is owner-only");
    assert_eq!(
        mode("ca-cert.pem"),
        0o644,
        "the certificate is world-readable"
    );
}

#[test]
fn generate_refuses_a_zero_validity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = generate(dir.path(), &["--validity-days", "0"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!dir.path().join("ca-cert.pem").exists());
}

#[test]
fn export_jks_and_pkcs12_hold_the_ca_and_merged_roots() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(generate(dir.path(), &[]).status.success());
    let cert = dir.path().join("ca-cert.pem");

    // Two roots in a bundle, named by their subject CN.
    let roots = tempfile::tempdir().expect("roots");
    let mut bundle = String::from("# a system bundle\n");
    for cn in ["Example Root A", "Example Root B"] {
        let sub = roots.path().join(cn.replace(' ', "_"));
        assert!(generate(&sub, &["--cn", cn]).status.success());
        bundle.push_str(&std::fs::read_to_string(sub.join("ca-cert.pem")).unwrap());
    }
    let bundle_path = roots.path().join("bundle.pem");
    std::fs::write(&bundle_path, bundle).expect("bundle");

    let jks = dir.path().join("ts.jks");
    let out = rift(&[
        "intercept-ca",
        "export",
        "--cert",
        arg(&cert),
        "--format",
        "jks",
        "--out",
        arg(&jks),
        "--merge-system-cas",
        arg(&bundle_path),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("javax.net.ssl.trustStore"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        jks_aliases(&std::fs::read(&jks).expect("jks")),
        vec!["rift-intercept-ca", "example root a", "example root b"]
    );

    let alone = dir.path().join("alone.jks");
    let out = rift(&[
        "intercept-ca",
        "export",
        "--cert",
        arg(&cert),
        "--format",
        "jks",
        "--out",
        arg(&alone),
        "--alias",
        "my-ca",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(jks_aliases(&std::fs::read(&alone).unwrap()), vec!["my-ca"]);

    let p12 = dir.path().join("ts.p12");
    let out = rift(&[
        "intercept-ca",
        "export",
        "--cert",
        arg(&cert),
        "--format",
        "pkcs12",
        "--out",
        arg(&p12),
        "--password",
        "s3cret",
        "--merge-system-cas",
        arg(&bundle_path),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let pfx = p12::PFX::parse(&std::fs::read(&p12).unwrap()).expect("parse p12");
    assert!(pfx.verify_mac("s3cret"), "protected by the given password");
    let certs = pfx.cert_bags("s3cret").expect("cert bags");
    assert_eq!(certs.len(), 3, "the CA plus both bundle roots");
    let ca_der = rustls_pemfile::certs(&mut std::fs::read_to_string(&cert).unwrap().as_bytes())
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(certs[0], ca_der.as_ref(), "the intercept CA comes first");
}

#[test]
fn export_refuses_an_existing_out_and_bad_inputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(generate(dir.path(), &[]).status.success());
    let cert = dir.path().join("ca-cert.pem");
    let out_path = dir.path().join("ts.jks");
    std::fs::write(&out_path, "keep me").unwrap();

    let export = |extra: &[&str]| {
        let mut args = vec!["intercept-ca", "export", "--format", "jks"];
        args.extend_from_slice(extra);
        rift(&args)
    };
    let out = export(&["--cert", arg(&cert), "--out", arg(&out_path)]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(&out_path).unwrap(), "keep me");
    assert!(
        export(&["--cert", arg(&cert), "--out", arg(&out_path), "--force"])
            .status
            .success()
    );

    let fresh = dir.path().join("fresh.jks");
    let missing = dir.path().join("nope.pem");
    assert_eq!(
        export(&["--cert", arg(&missing), "--out", arg(&fresh)])
            .status
            .code(),
        Some(2),
        "an unreadable cert is refused"
    );
    let junk = dir.path().join("junk.pem");
    std::fs::write(&junk, "not a bundle").unwrap();
    let out = export(&[
        "--cert",
        arg(&cert),
        "--out",
        arg(&fresh),
        "--merge-system-cas",
        arg(&junk),
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a bundle with no certificate is refused"
    );
    assert!(
        stderr(&out).contains("junk.pem"),
        "names the bad bundle: {}",
        stderr(&out)
    );
    assert!(!fresh.exists(), "a refused export writes nothing");
    assert_eq!(
        export(&[
            "--cert",
            arg(&cert),
            "--out",
            arg(&fresh),
            "--alias",
            "Upper"
        ])
        .status
        .code(),
        Some(2),
        "an alias a JKS cannot hold is refused"
    );
}

/// `--force` over a key that was world-readable must leave it owner-only: a replaced file keeps
/// its old mode unless the writer resets it.
#[cfg(unix)]
#[test]
fn generate_force_tightens_a_loose_key_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let key = dir.path().join("ca-key.pem");
    std::fs::write(&key, "old").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(dir.path().join("ca-cert.pem"), "old").unwrap();
    assert!(generate(dir.path(), &["--force"]).status.success());
    let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(
        std::fs::read_to_string(&key)
            .unwrap()
            .contains("PRIVATE KEY")
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no staging files left behind: {leftovers:?}"
    );
}

/// The flags reach the certificate: its CN and validity window are the ones asked for.
#[test]
fn generate_writes_the_requested_name_and_validity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let before = std::time::SystemTime::now();
    let out = generate(dir.path(), &["--cn", "Acme CI CA", "--validity-days", "30"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let pem = std::fs::read_to_string(dir.path().join("ca-cert.pem")).unwrap();
    let params = rcgen::CertificateParams::from_ca_cert_pem(&pem).expect("parse");
    assert_eq!(
        params.distinguished_name.get(&rcgen::DnType::CommonName),
        Some(&rcgen::DnValue::Utf8String("Acme CI CA".to_string()))
    );
    let now = before
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let lifetime = params.not_after.unix_timestamp() - now;
    assert!(
        (30 * 86_400 - 5..=30 * 86_400 + 60).contains(&lifetime),
        "valid for 30 days from now, got {lifetime}s"
    );
    assert!(params.not_before.unix_timestamp() <= now - 5 * 60);
}

#[test]
fn export_refuses_a_bundle_with_unsupported_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(generate(dir.path(), &[]).status.success());
    let cert = dir.path().join("ca-cert.pem");
    let pem = std::fs::read_to_string(&cert).unwrap();
    let bundle = dir.path().join("trust.crt");
    std::fs::write(
        &bundle,
        pem.replace("BEGIN CERTIFICATE", "BEGIN TRUSTED CERTIFICATE")
            .replace("END CERTIFICATE", "END TRUSTED CERTIFICATE"),
    )
    .unwrap();
    let out_path = dir.path().join("ts.jks");
    let out = rift(&[
        "intercept-ca",
        "export",
        "--cert",
        arg(&cert),
        "--format",
        "jks",
        "--out",
        arg(&out_path),
        "--merge-system-cas",
        arg(&bundle),
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(!out_path.exists());
}

/// A write that fails (here: the output directory does not exist) is exit 1, not a refusal.
#[test]
fn export_into_a_missing_directory_is_exit_1() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(generate(dir.path(), &[]).status.success());
    let cert = dir.path().join("ca-cert.pem");
    let out_path = dir.path().join("no/such/dir/ts.p12");
    let out = rift(&[
        "intercept-ca",
        "export",
        "--cert",
        arg(&cert),
        "--format",
        "pkcs12",
        "--out",
        arg(&out_path),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("ts.p12"),
        "names the path: {}",
        stderr(&out)
    );
}

/// The password can come from the environment, so it need not appear in argv.
#[test]
fn export_takes_the_password_from_the_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(generate(dir.path(), &[]).status.success());
    let cert = dir.path().join("ca-cert.pem");
    let p12 = dir.path().join("ts.p12");
    let out = Command::new(support::server_bin())
        .args([
            "intercept-ca",
            "export",
            "--cert",
            arg(&cert),
            "--format",
            "pkcs12",
        ])
        .args(["--out", arg(&p12)])
        .env("RIFT_TRUSTSTORE_PASSWORD", "from-env")
        .output()
        .expect("run rift");
    assert!(out.status.success(), "{}", stderr(&out));
    let pfx = p12::PFX::parse(&std::fs::read(&p12).unwrap()).expect("parse");
    assert!(pfx.verify_mac("from-env"));
    assert!(!pfx.verify_mac("changeit"));
}
