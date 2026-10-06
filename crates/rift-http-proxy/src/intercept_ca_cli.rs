//! `rift intercept-ca generate|export` (issue #1274).
//!
//! A SUT in its own container reads its truststore once at TLS init, so the intercept CA must exist
//! before either side starts. A running listener can mint one (`POST /intercept` with
//! `returnCaKey`), but nothing else could — every consumer re-implemented CA generation with
//! `keytool` or `openssl`. This is that step, built in: file work only, no listener, no admin API.
//!
//! Exit codes: `0` done; `2` refused — an output that already exists without `--force`, an input
//! that cannot be read or parsed, an invalid option; `1` generating or writing the output failed.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rift_mock_core::proxy::intercept_ca::{CertificateAuthority, GenerateOptions};
use rift_mock_core::proxy::truststore::{
    TrustStorePassword, export_jks_many, export_pkcs12_many, parse_pem_bundle,
    trust_entries_with_bundle,
};

use crate::server::{InterceptCaAction, TruststoreFormat};

const CERT_FILE: &str = "ca-cert.pem";
const KEY_FILE: &str = "ca-key.pem";

/// Why a run did not complete, which decides its exit code.
#[derive(Debug, thiserror::Error)]
enum CaCliError {
    /// The operator's input or the state on disk is wrong; nothing was written.
    #[error("{0}")]
    Refused(String),
    /// The input was valid but generating or writing the result failed.
    #[error("{0:#}")]
    Io(anyhow::Error),
}

impl CaCliError {
    fn exit_code(&self) -> i32 {
        match self {
            Self::Refused(_) => 2,
            Self::Io(_) => 1,
        }
    }
}

/// Run `action`, print its outcome, and exit with its code.
pub fn dispatch(action: InterceptCaAction) -> ! {
    let result = match action {
        InterceptCaAction::Generate {
            out_dir,
            cn,
            validity_days,
            force,
        } => generate(&out_dir, &cn, validity_days, force),
        InterceptCaAction::Export {
            cert,
            format,
            out,
            password,
            merge_system_cas,
            alias,
            force,
        } => export(&ExportArgs {
            cert: &cert,
            format,
            out: &out,
            password: TrustStorePassword::new(password),
            bundles: &merge_system_cas,
            alias: &alias,
            force,
        }),
    };
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(e.exit_code());
        }
    }
}

fn generate(out_dir: &Path, cn: &str, validity_days: u32, force: bool) -> Result<(), CaCliError> {
    if validity_days == 0 {
        return Err(CaCliError::Refused(
            "--validity-days must be at least 1".to_string(),
        ));
    }
    let cert_path = out_dir.join(CERT_FILE);
    let key_path = out_dir.join(KEY_FILE);
    // Both are checked before either is written: a leftover key must never end up paired with a
    // freshly generated certificate it does not belong to.
    if !force {
        for path in [&cert_path, &key_path] {
            if path.exists() {
                return Err(CaCliError::Refused(format!(
                    "{} already exists; pass --force to replace the CA pair",
                    path.display()
                )));
            }
        }
    }
    let ca = CertificateAuthority::generate_with(&GenerateOptions {
        common_name: cn.to_string(),
        validity: Duration::from_secs(u64::from(validity_days) * 86_400),
    })
    .map_err(CaCliError::Io)?;

    std::fs::create_dir_all(out_dir)
        .map_err(|e| CaCliError::Io(anyhow::anyhow!("create {}: {e}", out_dir.display())))?;
    write_pair(
        (&key_path, ca.ca_key_pem().as_bytes(), 0o600),
        (&cert_path, ca.ca_cert_pem().as_bytes(), 0o644),
        force,
    )?;

    println!("Wrote {}", cert_path.display());
    println!("Wrote {} (private key: keep it secret)", key_path.display());
    println!();
    println!("Start rift with this CA:");
    println!(
        "  rift --intercept-port 8080 --intercept-ca-cert {} --intercept-ca-key {}",
        cert_path.display(),
        key_path.display()
    );
    println!(
        "  or set RIFT_INTERCEPT_CA_CERT_PEM / RIFT_INTERCEPT_CA_KEY_PEM to the files' contents,"
    );
    println!("  or put \"caCertPath\" / \"caKeyPath\" in the config file's \"intercept\" block.");
    println!(
        "Give the SUT a truststore with: rift intercept-ca export --cert {}",
        cert_path.display()
    );
    Ok(())
}

struct ExportArgs<'a> {
    cert: &'a Path,
    format: TruststoreFormat,
    out: &'a Path,
    password: TrustStorePassword,
    bundles: &'a [PathBuf],
    alias: &'a str,
    force: bool,
}

fn export(args: &ExportArgs<'_>) -> Result<(), CaCliError> {
    if !args.force && args.out.exists() {
        return Err(CaCliError::Refused(format!(
            "{} already exists; pass --force to replace it",
            args.out.display()
        )));
    }
    let ca_der = parse_pem_bundle(&read_input(args.cert)?)
        .map_err(|e| CaCliError::Refused(format!("{}: {e:#}", args.cert.display())))?
        .into_iter()
        .next()
        .ok_or_else(|| CaCliError::Refused(format!("{}: no certificate", args.cert.display())))?;
    let mut bundle = Vec::new();
    for path in args.bundles {
        let certs = parse_pem_bundle(&read_input(path)?)
            .map_err(|e| CaCliError::Refused(format!("{}: {e:#}", path.display())))?;
        bundle.extend(certs);
    }
    let entries = trust_entries_with_bundle(args.alias, ca_der, bundle)
        .map_err(|e| CaCliError::Refused(format!("{e:#}")))?;
    let bytes = match args.format {
        TruststoreFormat::Jks => export_jks_many(&entries, &args.password),
        TruststoreFormat::Pkcs12 => export_pkcs12_many(&entries, &args.password),
    }
    .map_err(CaCliError::Io)?;
    write_file(args.out, &bytes, 0o644, args.force)?;

    let store_type = match args.format {
        TruststoreFormat::Jks => "JKS",
        TruststoreFormat::Pkcs12 => "PKCS12",
    };
    println!(
        "Wrote {} ({store_type}, {} certificate{})",
        args.out.display(),
        entries.len(),
        if entries.len() == 1 { "" } else { "s" }
    );
    println!(
        "JVM: -Djavax.net.ssl.trustStore={} -Djavax.net.ssl.trustStoreType={store_type} \
         -Djavax.net.ssl.trustStorePassword=<password>",
        args.out.display()
    );
    if args.bundles.is_empty() {
        println!(
            "Note: this store trusts the intercept CA only; a JVM using it as its default \
             truststore loses the public roots. Add --merge-system-cas <bundle.pem> to keep them."
        );
    }
    Ok(())
}

/// Write the key and certificate so that a failure never leaves a new key beside an old
/// certificate (or the reverse): both go to temporary files in the same directory first, and only
/// once both are fully written are they renamed into place.
fn write_pair(
    key: (&Path, &[u8], u32),
    cert: (&Path, &[u8], u32),
    force: bool,
) -> Result<(), CaCliError> {
    let staged = |(path, bytes, mode): (&Path, &[u8], u32)| -> Result<PathBuf, CaCliError> {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".tmp-{}", std::process::id()));
        let tmp = path.with_file_name(name);
        write_file(&tmp, bytes, mode, true)?;
        Ok(tmp)
    };
    let cleanup = |paths: &[&Path]| {
        for path in paths {
            // Best effort: the temporary file is ours, and the error being reported matters more.
            let _ = std::fs::remove_file(path);
        }
    };
    let key_tmp = staged(key)?;
    let cert_tmp = match staged(cert) {
        Ok(tmp) => tmp,
        Err(e) => {
            cleanup(&[&key_tmp]);
            return Err(e);
        }
    };
    // Without --force, re-check now that the slow part is done: a pair that appeared meanwhile is
    // still never overwritten.
    if !force && (key.0.exists() || cert.0.exists()) {
        cleanup(&[&key_tmp, &cert_tmp]);
        return Err(CaCliError::Refused(format!(
            "{} appeared while generating; pass --force to replace the CA pair",
            if key.0.exists() { key.0 } else { cert.0 }.display()
        )));
    }
    let rename = |from: &Path, to: &Path| {
        std::fs::rename(from, to)
            .map_err(|e| CaCliError::Io(anyhow::anyhow!("move {} into place: {e}", to.display())))
    };
    if let Err(e) = rename(&key_tmp, key.0) {
        cleanup(&[&key_tmp, &cert_tmp]);
        return Err(e);
    }
    rename(&cert_tmp, cert.0).inspect_err(|_| cleanup(&[&cert_tmp]))
}

fn read_input(path: &Path) -> Result<String, CaCliError> {
    std::fs::read_to_string(path)
        .map_err(|e| CaCliError::Refused(format!("cannot read {}: {e}", path.display())))
}

/// Write `bytes` to `path`. Without `force` the file is created exclusively, so a file that
/// appeared after the existence check is still never overwritten. `mode` applies on Unix.
fn write_file(path: &Path, bytes: &[u8], mode: u32, force: bool) -> Result<(), CaCliError> {
    let mut options = OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let io = |e: std::io::Error| CaCliError::Io(anyhow::anyhow!("write {}: {e}", path.display()));
    let mut file = options.open(path).map_err(io)?;
    // `mode` only applies when the file is created; a replaced file keeps its old mode, which for
    // the key could be looser than 0600.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(io)?;
    }
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)
}
