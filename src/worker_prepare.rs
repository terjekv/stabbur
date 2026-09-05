use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stabbur_domain::Sha256Digest;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const MANIFEST_SCHEMA_VERSION: u32 = 1;
const RECEIPT_SCHEMA_VERSION: u32 = 1;
const MAX_CONTROL_FILE_BYTES: u64 = 64 * 1024;
const MAX_PACKAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_PROCESS_OUTPUT_BYTES: usize = 64 * 1024;
const INSPECTION_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PrepareManifest {
    schema_version: u32,
    builder: String,
    version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replaces_manifest_sha256: Option<Sha256Digest>,
    package: InstallerPackage,
    health_check: HealthCheck,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallerPackage {
    path: PathBuf,
    sha256: Sha256Digest,
    identifier: String,
    signature: InstallerSignature,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallerSignature {
    policy: InstallerSignaturePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    team_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InstallerSignaturePolicy {
    DeveloperId,
    Unsigned,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HealthCheck {
    program: PathBuf,
    arguments: Vec<String>,
    expected_output: String,
}

#[derive(Serialize)]
struct InstalledManifestIdentity<'a> {
    schema_version: u32,
    builder: &'a str,
    version: &'a str,
    package: InstalledPackageIdentity<'a>,
    health_check: &'a HealthCheck,
}

#[derive(Serialize)]
struct InstalledPackageIdentity<'a> {
    sha256: &'a Sha256Digest,
    identifier: &'a str,
    signature: &'a InstallerSignature,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PrepareReceipt {
    schema_version: u32,
    builder: String,
    version: String,
    manifest_sha256: Sha256Digest,
    package_sha256: Sha256Digest,
    package_identifier: String,
    signature: InstallerSignature,
    expected_health_output: String,
    prepared_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PrepareReport {
    status: &'static str,
    builder: &'static str,
    version: String,
    manifest_sha256: Sha256Digest,
    package_sha256: Sha256Digest,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_version: Option<String>,
}

struct InstallationDisposition {
    status: &'static str,
    previous_version: Option<String>,
}

struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

pub(crate) async fn prepare(
    manifest_path: &Path,
    receipt_path: &Path,
    check_only: bool,
) -> Result<PrepareReport> {
    if !cfg!(target_os = "macos") {
        bail!("AutoPkg worker preparation is supported only on macOS");
    }
    require_absolute(manifest_path, "installer manifest")?;
    require_absolute(receipt_path, "preparation receipt")?;
    let manifest = read_manifest(manifest_path).await?;
    validate_manifest(&manifest)?;
    let manifest_sha256 = manifest_digest(&manifest)?;

    if check_only {
        inspect_package(&manifest.package.path, &manifest.package.sha256).await?;
        verify_package_signature(&manifest.package.path, &manifest.package.signature).await?;
        return Ok(report("verified", None, &manifest, &manifest_sha256));
    }

    require_root().await?;
    let current = read_receipt(receipt_path).await?;
    let exact_current = current
        .as_ref()
        .is_some_and(|receipt| receipt_matches(receipt, &manifest, &manifest_sha256));
    if exact_current
        && package_receipt_exists(&manifest.package.identifier).await?
        && health_check_succeeds(&manifest.health_check).await
    {
        return Ok(report(
            "already_prepared",
            None,
            &manifest,
            &manifest_sha256,
        ));
    }
    let disposition = authorize_install(current.as_ref(), exact_current, &manifest)?;

    inspect_package(&manifest.package.path, &manifest.package.sha256).await?;
    let staging = tempfile::Builder::new()
        .prefix("stabbur-worker-prepare-")
        .tempdir_in("/private/tmp")
        .context("creating private AutoPkg installer staging directory")?;
    let staged_package = staging.path().join("autopkg.pkg");
    tokio::fs::copy(&manifest.package.path, &staged_package)
        .await
        .context("staging the AutoPkg installer package")?;
    secure_staged_package(&staged_package).await?;
    verify_digest(&staged_package, &manifest.package.sha256).await?;
    verify_package_signature(&staged_package, &manifest.package.signature).await?;
    install_package(&staged_package).await?;

    if !package_receipt_exists(&manifest.package.identifier).await? {
        bail!("AutoPkg installer did not create the expected package receipt");
    }
    run_health_check(&manifest.health_check).await?;
    let receipt = PrepareReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        builder: manifest.builder.clone(),
        version: manifest.version.clone(),
        manifest_sha256,
        package_sha256: manifest.package.sha256.clone(),
        package_identifier: manifest.package.identifier.clone(),
        signature: manifest.package.signature.clone(),
        expected_health_output: manifest.health_check.expected_output.clone(),
        prepared_at: chrono::Utc::now(),
    };
    write_receipt(receipt_path, &receipt).await?;
    Ok(report(
        disposition.status,
        disposition.previous_version,
        &manifest,
        &receipt.manifest_sha256,
    ))
}

fn report(
    status: &'static str,
    previous_version: Option<String>,
    manifest: &PrepareManifest,
    manifest_sha256: &Sha256Digest,
) -> PrepareReport {
    PrepareReport {
        status,
        builder: "autopkg",
        version: manifest.version.clone(),
        manifest_sha256: manifest_sha256.clone(),
        package_sha256: manifest.package.sha256.clone(),
        previous_version,
    }
}

fn authorize_install(
    current: Option<&PrepareReceipt>,
    exact_current: bool,
    manifest: &PrepareManifest,
) -> Result<InstallationDisposition> {
    match current {
        None => {
            if manifest.replaces_manifest_sha256.is_some() {
                bail!(
                    "AutoPkg replacement precondition cannot be satisfied because no preparation receipt exists"
                );
            }
            Ok(InstallationDisposition {
                status: "installed",
                previous_version: None,
            })
        }
        Some(receipt) if exact_current => Ok(InstallationDisposition {
            status: "repaired",
            previous_version: Some(receipt.version.clone()),
        }),
        Some(receipt) => match manifest.replaces_manifest_sha256.as_ref() {
            Some(expected) if expected == &receipt.manifest_sha256 => Ok(InstallationDisposition {
                status: "replaced",
                previous_version: Some(receipt.version.clone()),
            }),
            Some(_) => bail!(
                "AutoPkg replacement precondition does not match installed manifest {}",
                receipt.manifest_sha256
            ),
            None => bail!(
                "a different AutoPkg manifest is installed; set replaces_manifest_sha256 to {} to authorize replacement",
                receipt.manifest_sha256
            ),
        },
    }
}

async fn read_manifest(path: &Path) -> Result<PrepareManifest> {
    let bytes = read_control_file(path, "installer manifest").await?;
    serde_json::from_slice(&bytes).context("parsing AutoPkg installer manifest")
}

fn validate_manifest(manifest: &PrepareManifest) -> Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        bail!("unsupported AutoPkg installer manifest schema version");
    }
    if manifest.builder != "autopkg" {
        bail!("AutoPkg installer manifest builder must be `autopkg`");
    }
    validate_opaque_version(&manifest.version)?;
    require_absolute(&manifest.package.path, "AutoPkg installer package")?;
    validate_identifier(&manifest.package.identifier)?;
    match manifest.package.signature.policy {
        InstallerSignaturePolicy::DeveloperId => {
            let team_id = manifest
                .package
                .signature
                .team_id
                .as_deref()
                .context("AutoPkg Developer ID signature policy requires a team ID")?;
            validate_team_id(team_id)?;
        }
        InstallerSignaturePolicy::Unsigned if manifest.package.signature.team_id.is_some() => {
            bail!("AutoPkg unsigned signature policy must not specify a team ID");
        }
        InstallerSignaturePolicy::Unsigned => {}
    }
    require_absolute(
        &manifest.health_check.program,
        "AutoPkg health-check program",
    )?;
    if manifest.health_check.arguments.len() > 16 {
        bail!("AutoPkg health check has too many arguments");
    }
    for argument in &manifest.health_check.arguments {
        if argument.len() > 1024 || argument.contains('\0') {
            bail!("AutoPkg health-check argument is invalid");
        }
    }
    validate_bounded_text(
        &manifest.health_check.expected_output,
        4096,
        "AutoPkg expected health-check output",
    )?;
    Ok(())
}

fn validate_opaque_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 256
        || version.trim() != version
        || version.chars().any(char::is_control)
    {
        bail!("AutoPkg version is invalid");
    }
    Ok(())
}

fn validate_bounded_text(value: &str, maximum: usize, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value
            .chars()
            .any(|character| character.is_control() && character != '\n')
    {
        bail!("{field} is invalid");
    }
    Ok(())
}

fn validate_identifier(identifier: &str) -> Result<()> {
    let valid = (1..=255).contains(&identifier.len())
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && identifier
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && identifier
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if !valid || !identifier.contains('.') {
        bail!("AutoPkg package identifier is invalid");
    }
    Ok(())
}

fn validate_team_id(team_id: &str) -> Result<()> {
    if team_id.len() != 10
        || !team_id
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        bail!("AutoPkg installer team ID must be 10 uppercase ASCII letters or digits");
    }
    Ok(())
}

fn require_absolute(path: &Path, field: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("{field} path must be absolute");
    }
    Ok(())
}

fn manifest_digest(manifest: &PrepareManifest) -> Result<Sha256Digest> {
    let identity = InstalledManifestIdentity {
        schema_version: manifest.schema_version,
        builder: &manifest.builder,
        version: &manifest.version,
        package: InstalledPackageIdentity {
            sha256: &manifest.package.sha256,
            identifier: &manifest.package.identifier,
            signature: &manifest.package.signature,
        },
        health_check: &manifest.health_check,
    };
    let canonical =
        serde_json::to_vec(&identity).context("canonicalizing installed AutoPkg identity")?;
    Sha256Digest::new(hex::encode(Sha256::digest(canonical)))
        .context("validating installer manifest digest")
}

async fn inspect_package(path: &Path, expected: &Sha256Digest) -> Result<()> {
    let metadata = secure_regular_file(path, "AutoPkg installer package").await?;
    if metadata.len() == 0 || metadata.len() > MAX_PACKAGE_BYTES {
        bail!("AutoPkg installer package size is invalid");
    }
    verify_digest(path, expected).await
}

async fn verify_digest(path: &Path, expected: &Sha256Digest) -> Result<()> {
    let mut file = tokio::fs::File::open(path)
        .await
        .context("opening AutoPkg installer package")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .context("reading AutoPkg installer package")?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(read).expect("buffer length fits u64"))
            .context("AutoPkg installer package is too large")?;
        if size > MAX_PACKAGE_BYTES {
            bail!("AutoPkg installer package is too large");
        }
        hasher.update(&buffer[..read]);
    }
    let actual = Sha256Digest::new(hex::encode(hasher.finalize()))
        .context("validating calculated installer digest")?;
    if &actual != expected {
        bail!("AutoPkg installer package SHA-256 does not match the manifest");
    }
    Ok(())
}

async fn verify_package_signature(path: &Path, policy: &InstallerSignature) -> Result<()> {
    let mut command = system_command(Path::new("/usr/sbin/pkgutil"));
    command.arg("--check-signature").arg(path);
    let output = capture(command, INSPECTION_TIMEOUT).await?;
    let stdout = std::str::from_utf8(&output.stdout)
        .context("Apple installer signature output is not UTF-8")?;
    let stderr = std::str::from_utf8(&output.stderr)
        .context("Apple installer signature diagnostic is not UTF-8")?;
    match policy.policy {
        InstallerSignaturePolicy::DeveloperId => {
            let team_id = policy
                .team_id
                .as_deref()
                .context("AutoPkg Developer ID signature policy requires a team ID")?;
            if !output.status.success() {
                bail!(
                    "AutoPkg installer package does not have a trusted Apple installer signature"
                );
            }
            if !signature_has_leaf_team_id(stdout, team_id)
                && !signature_has_leaf_team_id(stderr, team_id)
            {
                bail!("AutoPkg installer package signer does not match the manifest team ID");
            }
        }
        InstallerSignaturePolicy::Unsigned => {
            if output.status.success()
                || (!signature_is_unsigned(stdout) && !signature_is_unsigned(stderr))
            {
                bail!("AutoPkg installer package signature does not match the manifest policy");
            }
        }
    }
    Ok(())
}

fn signature_has_leaf_team_id(output: &str, expected_team_id: &str) -> bool {
    let suffix = format!("({expected_team_id})");
    output.lines().any(|line| {
        let line = line.trim();
        line.strip_prefix("1. ")
            .is_some_and(|certificate| certificate.trim_end().ends_with(&suffix))
    })
}

fn signature_is_unsigned(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim() == "Status: no signature")
}

async fn require_root() -> Result<()> {
    let mut command = system_command(Path::new("/usr/bin/id"));
    command.arg("-u");
    let output = capture(command, INSPECTION_TIMEOUT).await?;
    if !output.status.success() || std::str::from_utf8(&output.stdout).map(str::trim) != Ok("0") {
        bail!("worker prepare must be run as root");
    }
    Ok(())
}

async fn install_package(path: &Path) -> Result<()> {
    let mut command = system_command(Path::new("/usr/sbin/installer"));
    command
        .arg("-pkg")
        .arg(path)
        .arg("-target")
        .arg("/")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("starting the macOS package installer")?;
    let status = tokio::time::timeout(INSTALL_TIMEOUT, child.wait())
        .await
        .context("the macOS package installer timed out")?
        .context("waiting for the macOS package installer")?;
    if !status.success() {
        bail!("the macOS package installer failed");
    }
    Ok(())
}

async fn package_receipt_exists(identifier: &str) -> Result<bool> {
    let mut command = system_command(Path::new("/usr/sbin/pkgutil"));
    command
        .arg("--pkg-info")
        .arg(identifier)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("checking the AutoPkg package receipt")?;
    let status = tokio::time::timeout(INSPECTION_TIMEOUT, child.wait())
        .await
        .context("checking the AutoPkg package receipt timed out")?
        .context("waiting for the AutoPkg package receipt check")?;
    Ok(status.success())
}

async fn run_health_check(check: &HealthCheck) -> Result<()> {
    let program = resolve_health_program(&check.program).await?;
    let mut command = system_command(&program);
    command.args(&check.arguments);
    let output = capture(command, INSPECTION_TIMEOUT).await?;
    if !output.status.success() {
        bail!("installed AutoPkg health check failed");
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .context("AutoPkg health-check output is not UTF-8")?
        .trim();
    let stderr = std::str::from_utf8(&output.stderr)
        .context("AutoPkg health-check diagnostic is not UTF-8")?
        .trim();
    let detected = if stdout.is_empty() { stderr } else { stdout };
    if detected != check.expected_output {
        bail!("installed AutoPkg health-check output does not match the installer manifest");
    }
    Ok(())
}

async fn health_check_succeeds(check: &HealthCheck) -> bool {
    run_health_check(check).await.is_ok()
}

async fn resolve_health_program(path: &Path) -> Result<PathBuf> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .context("resolving installed AutoPkg program")?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .context("reading installed AutoPkg program metadata")?;
    if !metadata.is_file() {
        bail!("installed AutoPkg program is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 || mode & 0o022 != 0 {
            bail!("installed AutoPkg program permissions are unsafe");
        }
        if metadata.uid() != 0 {
            bail!("installed AutoPkg program must be owned by root");
        }
    }
    Ok(canonical)
}

fn receipt_matches(
    receipt: &PrepareReceipt,
    manifest: &PrepareManifest,
    manifest_sha256: &Sha256Digest,
) -> bool {
    receipt.schema_version == RECEIPT_SCHEMA_VERSION
        && receipt.builder == manifest.builder
        && receipt.version == manifest.version
        && &receipt.manifest_sha256 == manifest_sha256
        && receipt.package_sha256 == manifest.package.sha256
        && receipt.package_identifier == manifest.package.identifier
        && receipt.signature == manifest.package.signature
        && receipt.expected_health_output == manifest.health_check.expected_output
}

async fn read_receipt(path: &Path) -> Result<Option<PrepareReceipt>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            validate_control_file_metadata(&metadata, "preparation receipt")?;
            if metadata.len() > MAX_CONTROL_FILE_BYTES {
                bail!("preparation receipt is too large");
            }
            let bytes = tokio::fs::read(path)
                .await
                .context("reading AutoPkg preparation receipt")?;
            let receipt: PrepareReceipt =
                serde_json::from_slice(&bytes).context("parsing AutoPkg preparation receipt")?;
            if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
                bail!("unsupported AutoPkg preparation receipt schema version");
            }
            Ok(Some(receipt))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("reading AutoPkg preparation receipt metadata"),
    }
}

async fn write_receipt(path: &Path, receipt: &PrepareReceipt) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("preparation receipt must have a parent directory")?;
    ensure_private_directory(parent).await?;
    let temporary = parent.join(format!(".autopkg-prepared-{}.tmp", uuid::Uuid::now_v7()));
    let mut json = serde_json::to_vec_pretty(receipt).context("serializing preparation receipt")?;
    json.push(b'\n');
    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .await
            .context("creating AutoPkg preparation receipt")?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &json)
            .await
            .context("writing AutoPkg preparation receipt")?;
        file.sync_all()
            .await
            .context("syncing AutoPkg preparation receipt")?;
        drop(file);
        tokio::fs::rename(&temporary, path)
            .await
            .context("publishing AutoPkg preparation receipt")?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn ensure_private_directory(path: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                bail!("preparation receipt parent must be a real directory");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o022 != 0 {
                    bail!("preparation receipt parent must not be writable by group or world");
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(path)
                .await
                .context("creating preparation receipt directory")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                    .await
                    .context("securing preparation receipt directory")?;
            }
        }
        Err(error) => return Err(error).context("reading preparation receipt directory metadata"),
    }
    Ok(())
}

async fn read_control_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    let metadata = secure_regular_file(path, label).await?;
    if metadata.len() == 0 || metadata.len() > MAX_CONTROL_FILE_BYTES {
        bail!("{label} size is invalid");
    }
    tokio::fs::read(path)
        .await
        .with_context(|| format!("reading {label}"))
}

async fn secure_regular_file(path: &Path, label: &str) -> Result<std::fs::Metadata> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .with_context(|| format!("reading {label} metadata"))?;
    validate_control_file_metadata(&metadata, label)?;
    Ok(metadata)
}

fn validate_control_file_metadata(metadata: &std::fs::Metadata, label: &str) -> Result<()> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("{label} must be a regular non-symlink file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o022 != 0 {
            bail!("{label} must not be writable by group or world");
        }
    }
    Ok(())
}

async fn secure_staged_package(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .context("securing staged AutoPkg installer package")?;
    }
    let file = tokio::fs::File::open(path)
        .await
        .context("opening staged AutoPkg installer package")?;
    file.sync_all()
        .await
        .context("syncing staged AutoPkg installer package")
}

fn system_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("HOME", "/var/empty")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env(
            "PATH",
            "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        .kill_on_drop(true);
    command
}

async fn capture(mut command: Command, timeout: Duration) -> Result<CapturedOutput> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("starting local verification command")?;
    let stdout = child
        .stdout
        .take()
        .context("capturing local verification standard output")?;
    let stderr = child
        .stderr
        .take()
        .context("capturing local verification standard error")?;
    let captured = tokio::time::timeout(timeout, async {
        let wait = async {
            child
                .wait()
                .await
                .context("waiting for local verification command")
        };
        let (status, stdout, stderr) =
            tokio::try_join!(wait, read_bounded(stdout), read_bounded(stderr))?;
        Ok::<_, anyhow::Error>(CapturedOutput {
            status,
            stdout,
            stderr,
        })
    })
    .await
    .context("local verification command timed out")??;
    Ok(captured)
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .context("reading local verification output")?;
        if read == 0 {
            break;
        }
        if output.len().saturating_add(read) > MAX_PROCESS_OUTPUT_BYTES {
            bail!("local verification command output exceeded its limit");
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_AUTOPKG_FIXTURE: &str =
        include_str!("../tests/fixtures/autopkg-prepare/autopkg-2.9.0.json");

    fn manifest(package_path: PathBuf, digest: Sha256Digest) -> PrepareManifest {
        PrepareManifest {
            schema_version: 1,
            builder: "autopkg".into(),
            version: "1.2.3".into(),
            replaces_manifest_sha256: None,
            package: InstallerPackage {
                path: package_path,
                sha256: digest,
                identifier: "com.example.autopkg".into(),
                signature: InstallerSignature {
                    policy: InstallerSignaturePolicy::DeveloperId,
                    team_id: Some("ABC123DE45".into()),
                },
            },
            health_check: HealthCheck {
                program: PathBuf::from("/Library/AutoPkg/autopkg"),
                arguments: vec!["version".into()],
                expected_output: "1.2.3".into(),
            },
        }
    }

    #[test]
    fn installer_manifest_is_strict_and_bounded() {
        let digest = Sha256Digest::new("a".repeat(64)).unwrap();
        let valid = manifest(PathBuf::from("/private/tmp/autopkg.pkg"), digest);
        validate_manifest(&valid).unwrap();

        let mut invalid = serde_json::to_value(&valid).unwrap();
        invalid["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PrepareManifest>(invalid).is_err());

        let mut invalid_team = valid;
        invalid_team.package.signature = InstallerSignature {
            policy: InstallerSignaturePolicy::DeveloperId,
            team_id: Some("lowercase1".into()),
        };
        assert!(validate_manifest(&invalid_team).is_err());

        let digest = Sha256Digest::new("a".repeat(64)).unwrap();
        let mut missing_team = manifest(PathBuf::from("/private/tmp/autopkg.pkg"), digest);
        missing_team.package.signature.team_id = None;
        assert!(validate_manifest(&missing_team).is_err());

        let digest = Sha256Digest::new("a".repeat(64)).unwrap();
        let mut unsigned_with_team = manifest(PathBuf::from("/private/tmp/autopkg.pkg"), digest);
        unsigned_with_team.package.signature.policy = InstallerSignaturePolicy::Unsigned;
        assert!(validate_manifest(&unsigned_with_team).is_err());

        let digest = Sha256Digest::new("a".repeat(64)).unwrap();
        let mut invalid_version = manifest(PathBuf::from("/private/tmp/autopkg.pkg"), digest);
        invalid_version.version = "autopkg\n1.2.3".into();
        assert!(validate_manifest(&invalid_version).is_err());
        invalid_version.version = "1.2.3".into();
        invalid_version.health_check.expected_output.clear();
        assert!(validate_manifest(&invalid_version).is_err());
    }

    #[test]
    fn default_autopkg_fixture_matches_the_prepare_contract() {
        let fixture: serde_json::Value = serde_json::from_str(DEFAULT_AUTOPKG_FIXTURE).unwrap();
        assert_eq!(fixture["schema_version"], 1);
        assert_eq!(fixture["release"]["tag"], "v2.9.0");
        assert_eq!(fixture["release"]["size"], 52_253_994);
        assert!(
            fixture["release"]["url"]
                .as_str()
                .unwrap()
                .starts_with("https://")
        );

        let manifest: PrepareManifest =
            serde_json::from_value(fixture["manifest"].clone()).unwrap();
        validate_manifest(&manifest).unwrap();
        assert_eq!(
            manifest.package.sha256.as_str(),
            fixture["release"]["sha256"].as_str().unwrap()
        );
        assert_eq!(
            manifest_digest(&manifest).unwrap().as_str(),
            fixture["expected_manifest_sha256"].as_str().unwrap()
        );
    }

    #[test]
    fn manifest_digest_ignores_json_formatting() {
        let digest = Sha256Digest::new("b".repeat(64)).unwrap();
        let manifest = manifest(PathBuf::from("/private/tmp/autopkg.pkg"), digest);
        let compact: PrepareManifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();
        let pretty: PrepareManifest =
            serde_json::from_str(&serde_json::to_string_pretty(&manifest).unwrap()).unwrap();
        assert_eq!(
            manifest_digest(&compact).unwrap(),
            manifest_digest(&pretty).unwrap()
        );

        let mut relocated = pretty;
        relocated.package.path = PathBuf::from("/another/staging/location/autopkg.pkg");
        relocated.replaces_manifest_sha256 = Some(Sha256Digest::new("9".repeat(64)).unwrap());
        assert_eq!(
            manifest_digest(&compact).unwrap(),
            manifest_digest(&relocated).unwrap()
        );
    }

    #[test]
    fn only_the_leaf_installer_certificate_team_is_accepted() {
        let valid = "Package \"AutoPkg.pkg\":\n   Certificate Chain:\n    1. Developer ID Installer: Example (ABC123DE45)\n    2. Developer ID Certification Authority (WRONG00000)\n";
        assert!(signature_has_leaf_team_id(valid, "ABC123DE45"));
        assert!(!signature_has_leaf_team_id(valid, "WRONG00000"));
    }

    #[test]
    fn unsigned_signature_status_is_exact() {
        assert!(signature_is_unsigned(
            "Package \"autopkg.pkg\":\n   Status: no signature\n"
        ));
        assert!(!signature_is_unsigned("Status: signed by someone"));
    }

    #[test]
    fn unsigned_policy_is_explicit_and_part_of_the_identity() {
        let digest = Sha256Digest::new("6".repeat(64)).unwrap();
        let mut unsigned = manifest(PathBuf::from("/private/tmp/autopkg.pkg"), digest);
        let developer_id_digest = manifest_digest(&unsigned).unwrap();
        unsigned.package.signature = InstallerSignature {
            policy: InstallerSignaturePolicy::Unsigned,
            team_id: None,
        };
        validate_manifest(&unsigned).unwrap();
        assert_ne!(manifest_digest(&unsigned).unwrap(), developer_id_digest);

        let mut serialized = serde_json::to_value(&unsigned).unwrap();
        assert_eq!(serialized["package"]["signature"]["policy"], "unsigned");
        serialized["package"]["signature"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PrepareManifest>(serialized).is_err());
    }

    #[tokio::test]
    async fn package_digest_mismatch_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let package = temporary.path().join("autopkg.pkg");
        tokio::fs::write(&package, b"not the expected package")
            .await
            .unwrap();
        let expected = Sha256Digest::new("0".repeat(64)).unwrap();
        let error = verify_digest(&package, &expected).await.unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn receipt_replay_requires_the_exact_canonical_manifest() {
        let package_digest = Sha256Digest::new("c".repeat(64)).unwrap();
        let manifest = manifest(
            PathBuf::from("/private/tmp/autopkg.pkg"),
            package_digest.clone(),
        );
        let manifest_sha256 = manifest_digest(&manifest).unwrap();
        let receipt = PrepareReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            builder: "autopkg".into(),
            version: manifest.version.clone(),
            manifest_sha256: manifest_sha256.clone(),
            package_sha256: package_digest,
            package_identifier: manifest.package.identifier.clone(),
            signature: manifest.package.signature.clone(),
            expected_health_output: manifest.health_check.expected_output.clone(),
            prepared_at: chrono::Utc::now(),
        };
        assert!(receipt_matches(&receipt, &manifest, &manifest_sha256));

        let mut changed = manifest;
        changed.health_check.arguments.push("--json".into());
        assert!(!receipt_matches(
            &receipt,
            &changed,
            &manifest_digest(&changed).unwrap()
        ));
    }

    #[test]
    fn replacements_require_the_exact_installed_manifest_digest() {
        let package_digest = Sha256Digest::new("d".repeat(64)).unwrap();
        let current_manifest = manifest(
            PathBuf::from("/private/tmp/autopkg-1.pkg"),
            package_digest.clone(),
        );
        let current_digest = manifest_digest(&current_manifest).unwrap();
        let current = PrepareReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            builder: "autopkg".into(),
            version: current_manifest.version.clone(),
            manifest_sha256: current_digest.clone(),
            package_sha256: package_digest,
            package_identifier: current_manifest.package.identifier.clone(),
            signature: current_manifest.package.signature.clone(),
            expected_health_output: current_manifest.health_check.expected_output.clone(),
            prepared_at: chrono::Utc::now(),
        };

        let mut replacement = manifest(
            PathBuf::from("/private/tmp/autopkg-2.pkg"),
            Sha256Digest::new("e".repeat(64)).unwrap(),
        );
        replacement.version = "2.0".into();
        replacement.health_check.expected_output = "2.0".into();
        assert!(authorize_install(Some(&current), false, &replacement).is_err());

        replacement.replaces_manifest_sha256 = Some(Sha256Digest::new("f".repeat(64)).unwrap());
        assert!(authorize_install(Some(&current), false, &replacement).is_err());

        replacement.replaces_manifest_sha256 = Some(current_digest);
        let authorized = authorize_install(Some(&current), false, &replacement).unwrap();
        assert_eq!(authorized.status, "replaced");
        assert_eq!(authorized.previous_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn first_install_and_exact_repair_have_distinct_preconditions() {
        let package_digest = Sha256Digest::new("1".repeat(64)).unwrap();
        let mut initial = manifest(
            PathBuf::from("/private/tmp/autopkg.pkg"),
            package_digest.clone(),
        );
        assert_eq!(
            authorize_install(None, false, &initial).unwrap().status,
            "installed"
        );

        initial.replaces_manifest_sha256 = Some(Sha256Digest::new("2".repeat(64)).unwrap());
        assert!(authorize_install(None, false, &initial).is_err());

        let receipt = PrepareReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            builder: "autopkg".into(),
            version: initial.version.clone(),
            manifest_sha256: manifest_digest(&initial).unwrap(),
            package_sha256: package_digest,
            package_identifier: initial.package.identifier.clone(),
            signature: initial.package.signature.clone(),
            expected_health_output: initial.health_check.expected_output.clone(),
            prepared_at: chrono::Utc::now(),
        };
        let repair = authorize_install(Some(&receipt), true, &initial).unwrap();
        assert_eq!(repair.status, "repaired");
    }
}
