//! `zirv update` installs a release-matched binary over the currently
//! running executable. Platform selection and version parsing happen before
//! any network call; the downloaded binary is executed and verified before
//! an atomic replacement is attempted.

use std::cmp::Ordering;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Parser;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::report::GITHUB_REPOSITORY;

type UpdateResult<T> = Result<T, Box<dyn std::error::Error>>;

const API_TIMEOUT_SECS: u64 = 15;
const CONNECT_TIMEOUT_SECS: u64 = 5;
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;
const SANITY_TIMEOUT_SECS: u64 = 30;
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "zirv update",
    about = "Install a zirv release over the currently running binary.",
    disable_help_subcommand = true,
    disable_version_flag = true
)]
pub struct UpdateCli {
    /// Release to install, with an optional leading `v`.
    #[arg(long, value_name = "x.y.z")]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

static HTTP_AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();

fn http_agent() -> &'static ureq::Agent {
    HTTP_AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(API_TIMEOUT_SECS)))
            .timeout_connect(Some(Duration::from_secs(CONNECT_TIMEOUT_SECS)))
            .build()
            .into()
    })
}

fn normalize_version(version: &str) -> UpdateResult<String> {
    let version = version.strip_prefix('v').unwrap_or(version);
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(format!(
            "invalid version '{version}'; expected N.N.N (an optional leading v is accepted)"
        )
        .into());
    }
    Ok(version.to_string())
}

fn version_from_release_json(json: &str) -> UpdateResult<String> {
    let release: LatestRelease = serde_json::from_str(json)
        .map_err(|error| format!("GitHub returned an unreadable latest release: {error}"))?;
    normalize_version(&release.tag_name)
        .map_err(|error| format!("GitHub returned an invalid release tag: {error}").into())
}

fn asset_name(os: &str, arch: &str, version: &str) -> UpdateResult<String> {
    match os {
        "linux" if arch == "x86_64" => Ok(format!("zirv-{version}-linux.tar.gz")),
        "linux" => Err(format!(
            "Unsupported architecture for Linux: {arch}.\n\
             The prebuilt Linux release is x86_64-only. Build from source instead:\n  \
             cargo install --git https://github.com/{GITHUB_REPOSITORY}"
        )
        .into()),
        "macos" if matches!(arch, "x86_64" | "aarch64") => {
            Ok(format!("zirv-{version}-macos.tar.gz"))
        }
        "macos" => Err(format!("Unsupported architecture for macOS: {arch}.").into()),
        "windows" => Ok(format!("zirv-{version}-windows.exe")),
        _ => Err(format!("Unsupported operating system: {os}").into()),
    }
}

fn asset_url(os: &str, arch: &str, version: &str) -> UpdateResult<String> {
    let asset = asset_name(os, arch, version)?;
    Ok(format!(
        "https://github.com/{GITHUB_REPOSITORY}/releases/download/v{version}/{asset}"
    ))
}

fn releases_url() -> String {
    format!("https://github.com/{GITHUB_REPOSITORY}/releases")
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    for (left, right) in left.split('.').zip(right.split('.')) {
        let left = left.trim_start_matches('0');
        let right = right.trim_start_matches('0');
        let ordering = left.len().cmp(&right.len()).then_with(|| left.cmp(right));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn update_skip_reason(explicit_version: bool, target: &str, current: &str) -> Option<String> {
    if explicit_version {
        return None;
    }
    match compare_versions(current, target) {
        Ordering::Less => None,
        Ordering::Equal => Some(format!("zirv {current} is already the latest release")),
        Ordering::Greater => Some(format!(
            "zirv {current} is newer than the latest release {target}; pass --version to install a specific release"
        )),
    }
}

fn latest_release_json() -> UpdateResult<String> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPOSITORY}/releases/latest");
    let mut response = http_agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", &format!("zirv/{}", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|error| format!("could not resolve the latest zirv release: {error}"))?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("GitHub returned an unreadable latest release: {error}").into())
}

/// Downloads the `.sha256` sidecar for a release asset. Returns `Ok(None)`
/// for a 404 (an older release published before checksums existed) rather
/// than erroring, so the caller can fail closed with a specific message
/// instead of the generic "asset not found" wording `download_asset` uses
/// for the binary itself.
fn download_checksum(url: &str) -> UpdateResult<Option<Vec<u8>>> {
    let response = http_agent()
        .get(url)
        .header("Accept", "text/plain")
        .header("User-Agent", &format!("zirv/{}", env!("CARGO_PKG_VERSION")))
        .config()
        .timeout_global(Some(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS)))
        .build()
        .call();
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(error) => return Err(format!("could not download {url}: {error}").into()),
    };
    let bytes = response
        .body_mut()
        .with_config()
        // A `<hex>  <filename>` line is well under a kilobyte; this only
        // guards against a misbehaving/compromised server streaming
        // something enormous instead of a checksum file.
        .limit(4096)
        .read_to_vec()
        .map_err(|error| format!("could not read the downloaded checksum file: {error}"))?;
    Ok(Some(bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Parses a `sha256sum`-format checksum file (`<64-hex-char digest>  <filename>`,
/// one entry per line -- only the first line is read) and verifies `bytes`
/// against it. Pure: no I/O, so every failure mode is unit-testable without
/// a network stub.
fn verify_checksum(bytes: &[u8], sha256_text: &str, expected_name: &str) -> UpdateResult<()> {
    let line = sha256_text
        .lines()
        .next()
        .ok_or("downloaded checksum file is empty")?;
    let mut fields = line.split_whitespace();
    let digest = fields
        .next()
        .ok_or_else(|| format!("malformed checksum line (no digest field): '{line}'"))?;
    let name = fields
        .next()
        .ok_or_else(|| format!("malformed checksum line (no filename field): '{line}'"))?;
    if fields.next().is_some() {
        return Err(format!("malformed checksum line (unexpected extra field): '{line}'").into());
    }
    // `sha256sum -b` prefixes the filename with `*` for binary mode.
    let name = name.trim_start_matches('*');
    if name != expected_name {
        return Err(format!(
            "checksum file names an unexpected asset: expected '{expected_name}', got '{name}'"
        )
        .into());
    }
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "malformed checksum line: '{digest}' is not a 64-character hex digest"
        )
        .into());
    }
    let expected_digest = digest.to_ascii_lowercase();
    let actual_digest = to_hex(&Sha256::digest(bytes));
    if actual_digest != expected_digest {
        return Err(format!(
            "checksum mismatch for {expected_name}: expected {expected_digest}, computed {actual_digest}; \
             the download may be corrupted or tampered with -- not installing it"
        )
        .into());
    }
    Ok(())
}

fn download_asset(url: &str) -> UpdateResult<Vec<u8>> {
    let response = http_agent()
        .get(url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", &format!("zirv/{}", env!("CARGO_PKG_VERSION")))
        .config()
        .timeout_global(Some(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS)))
        .build()
        .call();
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => {
            return Err(format!(
                "release asset not found; that release/version does not exist. See {}",
                releases_url()
            )
            .into());
        }
        Err(error) => return Err(format!("could not download {url}: {error}").into()),
    };
    response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD_BYTES as u64)
        .read_to_vec()
        .map_err(|error| format!("could not read the downloaded release asset: {error}").into())
}

#[cfg(unix)]
fn binary_from_asset(asset: &[u8]) -> UpdateResult<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(asset);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("downloaded release archive is invalid: {error}"))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|error| format!("downloaded release archive is invalid: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("downloaded release archive has an invalid path: {error}"))?
            .into_owned();
        let is_binary = path == Path::new("zirv") || path == Path::new("./zirv");
        if !is_binary || !entry.header().entry_type().is_file() {
            continue;
        }
        if entry.size() > MAX_DOWNLOAD_BYTES as u64 {
            return Err("downloaded zirv executable is unexpectedly large".into());
        }
        let mut binary = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut binary)
            .map_err(|error| format!("could not extract the downloaded zirv binary: {error}"))?;
        return Ok(binary);
    }
    Err("downloaded release archive does not contain a regular zirv executable".into())
}

#[cfg(windows)]
fn binary_from_asset(asset: &[u8]) -> UpdateResult<Vec<u8>> {
    Ok(asset.to_vec())
}

fn permission_hint() -> &'static str {
    if cfg!(windows) {
        "run from an elevated shell, or `choco upgrade zirv` if installed with Chocolatey"
    } else {
        "re-run with sudo, or `brew upgrade zirv` if installed with Homebrew"
    }
}

fn path_error(action: &str, path: &Path, error: io::Error) -> Box<dyn std::error::Error> {
    if error.kind() == io::ErrorKind::PermissionDenied {
        format!(
            "could not write {}: {error}; {}",
            path.display(),
            permission_hint()
        )
        .into()
    } else {
        format!("could not {action} {}: {error}", path.display()).into()
    }
}

fn write_binary(path: &Path, bytes: &[u8]) -> UpdateResult<()> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o755);
    let mut file = options
        .open(path)
        .map_err(|error| path_error("create", path, error))?;
    file.write_all(bytes)
        .map_err(|error| path_error("write", path, error))?;
    file.sync_all()
        .map_err(|error| path_error("sync", path, error))?;
    drop(file);
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|error| path_error("set permissions on", path, error))?;
    Ok(())
}

fn read_capped(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(kept)
}

fn rendered_output(stdout: &[u8], stderr: &[u8]) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(stdout).trim(),
        String::from_utf8_lossy(stderr).trim()
    )
}

fn sanity_check(path: &Path, version: &str) -> UpdateResult<()> {
    let mut child = Command::new(path)
        .arg("version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!(
                "could not run downloaded binary {}: {error}",
                path.display()
            )
        })?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("could not capture downloaded binary stdout".into());
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("could not capture downloaded binary stderr".into());
    };
    let stdout_reader = std::thread::spawn(move || read_capped(stdout));
    let stderr_reader = std::thread::spawn(move || read_capped(stderr));
    let deadline = Instant::now() + Duration::from_secs(SANITY_TIMEOUT_SECS);
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not wait for downloaded binary: {error}").into());
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "downloaded binary stdout reader failed")??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "downloaded binary stderr reader failed")??;
    let actual = rendered_output(&stdout, &stderr);
    if timed_out {
        return Err(format!("downloaded binary timed out during sanity check; {actual}").into());
    }
    if !status.is_some_and(|status| status.success()) {
        return Err(format!("downloaded binary failed its sanity check; {actual}").into());
    }
    let stdout = String::from_utf8_lossy(&stdout);
    if !stdout.contains(&format!("Version: {version}")) {
        return Err(
            format!("downloaded binary did not report target version {version}; {actual}").into(),
        );
    }
    Ok(())
}

#[cfg(unix)]
fn replace_binary(target: &Path, binary: &[u8], version: &str) -> UpdateResult<()> {
    let dir = target.parent().ok_or_else(|| {
        format!(
            "installed binary has no parent directory: {}",
            target.display()
        )
    })?;
    let temp = dir.join(format!(".zirv-update-{}", std::process::id()));
    let result = (|| {
        write_binary(&temp, binary)?;
        sanity_check(&temp, version)?;
        fs::rename(&temp, target).map_err(|error| path_error("replace", target, error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(windows)]
fn replace_binary(target: &Path, binary: &[u8], version: &str) -> UpdateResult<()> {
    let dir = target.parent().ok_or_else(|| {
        format!(
            "installed binary has no parent directory: {}",
            target.display()
        )
    })?;
    let new = dir.join("zirv.exe.new");
    let old = dir.join("zirv.exe.old");
    let _ = fs::remove_file(&old);
    let result = (|| {
        write_binary(&new, binary)?;
        sanity_check(&new, version)?;
        fs::rename(target, &old).map_err(|error| path_error("rename", target, error))?;
        if let Err(error) = fs::rename(&new, target) {
            let rollback = fs::rename(&old, target);
            return match rollback {
                Ok(()) => Err(path_error("replace", target, error)),
                Err(rollback_error) => {
                    let replace_error = path_error("replace", target, error);
                    let rollback_error = path_error("restore", target, rollback_error);
                    Err(format!("{replace_error}; rollback also failed: {rollback_error}").into())
                }
            };
        }
        let _ = fs::remove_file(&old);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&new);
    }
    result
}

type LatestReleaseFn = dyn Fn() -> UpdateResult<String>;
type DownloadFn = dyn Fn(&str) -> UpdateResult<Vec<u8>>;
type ChecksumDownloadFn = dyn Fn(&str) -> UpdateResult<Option<Vec<u8>>>;
type InstallFn = dyn Fn(&Path, &[u8], &str) -> UpdateResult<()>;

struct UpdateContext<'a> {
    current_version: &'a str,
    os: &'a str,
    arch: &'a str,
    target_path: &'a Path,
    latest_release: &'a LatestReleaseFn,
    downloader: &'a DownloadFn,
    checksum_downloader: &'a ChecksumDownloadFn,
    installer: &'a InstallFn,
}

fn update_in(cli: &UpdateCli, context: &UpdateContext<'_>) -> UpdateResult<i32> {
    asset_name(context.os, context.arch, context.current_version)?;
    let explicit = cli.version.is_some();
    let target_version = match cli.version.as_deref() {
        Some(version) => normalize_version(version)?,
        None => version_from_release_json(&(context.latest_release)()?)?,
    };
    if let Some(reason) = update_skip_reason(explicit, &target_version, context.current_version) {
        crate::output::note(reason);
        return Ok(0);
    }
    let asset = asset_name(context.os, context.arch, &target_version)?;
    let url = asset_url(context.os, context.arch, &target_version)?;
    crate::output::note(format!(
        "Updating zirv {} to {target_version}",
        context.current_version,
    ));
    crate::output::note(format!("Downloading {url}"));
    let downloaded = (context.downloader)(&url)?;

    let checksum_url = format!("{url}.sha256");
    match (context.checksum_downloader)(&checksum_url)? {
        Some(bytes) => {
            let text = String::from_utf8(bytes)
                .map_err(|_| format!("checksum file at {checksum_url} is not valid UTF-8"))?;
            verify_checksum(&downloaded, &text, &asset)?;
            crate::output::note("Checksum verified");
        }
        None => {
            return Err(format!(
                "release v{target_version} publishes no checksum for {asset} \
                 ({checksum_url} returned 404); refusing to install an unverified binary. \
                 Pass --version to install a release that publishes one."
            )
            .into());
        }
    }

    let binary = binary_from_asset(&downloaded)?;
    (context.installer)(context.target_path, &binary, &target_version)?;
    crate::output::success(format!(
        "zirv {target_version} installed to {}",
        context.target_path.display()
    ));
    Ok(0)
}

/// `args[0]` is the literal `update` command as it appeared in argv. It is
/// discarded so case-insensitive raw dispatch retains stable clap usage text.
pub fn dispatch(args: &[String]) -> i32 {
    let argv = std::iter::once("zirv update".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match UpdateCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
        }
    };
    let target = match std::env::current_exe()
        .map_err(|error| format!("could not locate the running zirv binary: {error}"))
        .and_then(|path| {
            path.canonicalize().map_err(|error| {
                format!(
                    "could not resolve the running zirv binary {}: {error}",
                    path.display()
                )
            })
        }) {
        Ok(path) => path,
        Err(error) => {
            crate::output::error(error);
            return 1;
        }
    };
    let context = UpdateContext {
        current_version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        target_path: &target,
        latest_release: &latest_release_json,
        downloader: &download_asset,
        checksum_downloader: &download_checksum,
        installer: &replace_binary,
    };
    match update_in(&cli, &context) {
        Ok(code) => code,
        Err(error) => {
            crate::output::error(error);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_accept_three_numeric_components_and_optional_v() {
        assert_eq!(normalize_version("3.2.1").unwrap(), "3.2.1");
        assert_eq!(normalize_version("v3.2.1").unwrap(), "3.2.1");
        for invalid in ["3.2", "abc", "3.2.1.0", "3.x.1", "V3.2.1"] {
            assert!(normalize_version(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn release_json_extracts_and_normalizes_tag_name() {
        assert_eq!(
            version_from_release_json(r#"{"tag_name":"v3.20.0"}"#).unwrap(),
            "3.20.0"
        );
        assert!(version_from_release_json(r#"{"name":"release"}"#).is_err());
    }

    #[test]
    fn assets_match_supported_platforms_and_release_urls() {
        assert_eq!(
            asset_url("linux", "x86_64", "3.20.0").unwrap(),
            "https://github.com/Glubiz/zirv-cli/releases/download/v3.20.0/zirv-3.20.0-linux.tar.gz"
        );
        assert_eq!(
            asset_name("macos", "aarch64", "3.20.0").unwrap(),
            "zirv-3.20.0-macos.tar.gz"
        );
        assert_eq!(
            asset_name("windows", "x86_64", "3.20.0").unwrap(),
            "zirv-3.20.0-windows.exe"
        );
        assert!(asset_name("linux", "aarch64", "3.20.0").is_err());
        assert!(asset_name("freebsd", "x86_64", "3.20.0").is_err());
    }

    #[test]
    fn implicit_updates_only_when_current_is_older() {
        assert_eq!(update_skip_reason(false, "3.10.0", "3.9.0"), None);
        assert_eq!(
            update_skip_reason(false, "3.20.0", "3.20.0").as_deref(),
            Some("zirv 3.20.0 is already the latest release")
        );
        assert_eq!(
            update_skip_reason(false, "3.20.0", "3.100.0").as_deref(),
            Some(
                "zirv 3.100.0 is newer than the latest release 3.20.0; pass --version to install a specific release"
            )
        );
        assert_eq!(update_skip_reason(true, "3.19.9", "3.20.0"), None);
    }

    #[test]
    fn unsupported_platform_fails_before_transport_is_called() {
        let cli = UpdateCli { version: None };
        let context = UpdateContext {
            current_version: "3.20.0",
            os: "linux",
            arch: "aarch64",
            target_path: Path::new("/unused/zirv"),
            latest_release: &|| panic!("latest release transport must not run"),
            downloader: &|_| panic!("download transport must not run"),
            checksum_downloader: &|_| panic!("checksum transport must not run"),
            installer: &|_, _, _| panic!("installer must not run"),
        };
        let error = update_in(&cli, &context)
            .expect_err("unsupported platform must fail")
            .to_string();
        assert!(error.contains("cargo install --git"), "got {error}");
    }

    #[test]
    fn verify_checksum_accepts_a_matching_digest() {
        let bytes = b"known binary bytes";
        let digest = to_hex(&Sha256::digest(bytes));
        let text = format!("{digest}  zirv-3.30.0-linux.tar.gz\n");
        verify_checksum(bytes, &text, "zirv-3.30.0-linux.tar.gz").unwrap();
    }

    #[test]
    fn verify_checksum_accepts_uppercase_hex() {
        let bytes = b"known binary bytes";
        let digest = to_hex(&Sha256::digest(bytes)).to_ascii_uppercase();
        let text = format!("{digest}  zirv-3.30.0-linux.tar.gz\n");
        verify_checksum(bytes, &text, "zirv-3.30.0-linux.tar.gz").unwrap();
    }

    #[test]
    fn verify_checksum_rejects_a_mismatched_digest() {
        let bytes = b"known binary bytes";
        let wrong_digest = to_hex(&Sha256::digest(b"different bytes"));
        let text = format!("{wrong_digest}  zirv-3.30.0-linux.tar.gz\n");
        let error = verify_checksum(bytes, &text, "zirv-3.30.0-linux.tar.gz")
            .expect_err("mismatched digest must fail")
            .to_string();
        assert!(error.contains("checksum mismatch"), "got {error}");
        assert!(error.contains(&wrong_digest), "got {error}");
    }

    #[test]
    fn verify_checksum_rejects_malformed_text() {
        for text in [
            "",
            "not-a-real-checksum-line",
            "abc123  zirv-3.30.0-linux.tar.gz",
        ] {
            let error = verify_checksum(b"bytes", text, "zirv-3.30.0-linux.tar.gz")
                .expect_err(&format!("'{text}' must be rejected as malformed"))
                .to_string();
            assert!(
                error.contains("malformed") || error.contains("empty"),
                "got {error} for input {text:?}"
            );
        }
    }

    #[test]
    fn verify_checksum_rejects_extra_fields() {
        let bytes = b"known binary bytes";
        let digest = to_hex(&Sha256::digest(bytes));
        let text = format!("{digest}  zirv-3.30.0-linux.tar.gz  extra\n");
        let error = verify_checksum(bytes, &text, "zirv-3.30.0-linux.tar.gz")
            .expect_err("an extra field must be rejected")
            .to_string();
        assert!(error.contains("malformed"), "got {error}");
    }

    #[test]
    fn verify_checksum_rejects_a_filename_mismatch() {
        let bytes = b"known binary bytes";
        let digest = to_hex(&Sha256::digest(bytes));
        let text = format!("{digest}  zirv-3.30.0-windows.exe\n");
        let error = verify_checksum(bytes, &text, "zirv-3.30.0-linux.tar.gz")
            .expect_err("a filename mismatch must fail")
            .to_string();
        assert!(error.contains("unexpected asset"), "got {error}");
    }

    #[cfg(unix)]
    fn tar_gz(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for &(path, entry_type, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(entry_type);
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append_data(&mut header, path, contents).unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn unix_archive_extracts_zirv_and_ignores_other_entries() {
        for binary_path in ["zirv", "./zirv"] {
            let entries: [(&str, tar::EntryType, &[u8]); 3] = [
                ("._zirv", tar::EntryType::Regular, b"sidecar"),
                (binary_path, tar::EntryType::Regular, b"known binary bytes"),
                ("README", tar::EntryType::Regular, b"ignored"),
            ];
            let asset = tar_gz(&entries);

            assert_eq!(binary_from_asset(&asset).unwrap(), b"known binary bytes");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_archive_rejects_missing_or_non_file_zirv() {
        let missing_entries: [(&str, tar::EntryType, &[u8]); 1] =
            [("._zirv", tar::EntryType::Regular, b"sidecar")];
        let directory_entries: [(&str, tar::EntryType, &[u8]); 1] =
            [("zirv", tar::EntryType::Directory, b"")];

        for asset in [tar_gz(&missing_entries), tar_gz(&directory_entries)] {
            let error = binary_from_asset(&asset)
                .expect_err("archive must contain a regular zirv executable")
                .to_string();
            assert!(error.contains("regular zirv executable"), "got {error}");
        }
    }

    #[cfg(unix)]
    fn version_script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\nprintf 'Version: {version}\\n'\n").into_bytes()
    }

    #[cfg(unix)]
    #[test]
    fn unix_replace_sanity_checks_then_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("zirv");
        fs::write(&target, b"old binary").unwrap();

        replace_binary(&target, &version_script("9.8.7"), "9.8.7").unwrap();

        assert_eq!(fs::read(&target).unwrap(), version_script("9.8.7"));
        assert!(
            !dir.path()
                .join(format!(".zirv-update-{}", std::process::id()))
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_replace_cleans_up_and_preserves_target_on_sanity_failure() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("zirv");
        fs::write(&target, b"old binary").unwrap();

        let error = replace_binary(&target, &version_script("1.0.0"), "9.8.7")
            .expect_err("wrong version must fail")
            .to_string();

        assert!(error.contains("Version: 1.0.0"), "got {error}");
        assert_eq!(fs::read(&target).unwrap(), b"old binary");
        assert!(
            !dir.path()
                .join(format!(".zirv-update-{}", std::process::id()))
                .exists()
        );
    }
}
