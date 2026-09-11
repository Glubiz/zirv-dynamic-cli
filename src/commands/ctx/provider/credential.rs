use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::super::config::EnvLookup;

const STORE_TIMEOUT_SECS: u64 = 3;
const HARNESS_REFUSAL: &str =
    "harness login tokens are subscription entitlements, not API credentials -- use an API key";

#[derive(Clone, PartialEq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Returns the credential value. N07/N08 transports are the intended
    /// production callers; diagnostics and serialization must never use it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialRef {
    Env(String),
    Store(String),
    File(PathBuf),
}

impl CredentialRef {
    pub fn store_item(&self) -> Option<&str> {
        match self {
            Self::Store(item) => Some(item),
            _ => None,
        }
    }
}

impl std::str::FromStr for CredentialRef {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(name) = value.strip_prefix("env:") {
            if valid_env_name(name) {
                return Ok(Self::Env(name.to_string()));
            }
            return Err(format!(
                "invalid credential ref {value:?}: env name must match [A-Za-z_][A-Za-z0-9_]* and never contain `=`"
            ));
        }
        if let Some(item) = value.strip_prefix("store:") {
            if item == "Claude Code-credentials" || valid_item(item) {
                return Ok(Self::Store(item.to_string()));
            }
            return Err(format!(
                "invalid credential ref {value:?}: store item must be a lower-case slug"
            ));
        }
        if let Some(path) = value.strip_prefix("file:") {
            if path.is_empty() {
                return Err(format!(
                    "invalid credential ref {value:?}: file path is empty"
                ));
            }
            return Ok(Self::File(expand_tilde(path)?));
        }
        Err(format!(
            "invalid credential ref {value:?}; expected env:NAME, store:<item>, or file:<path>"
        ))
    }
}

impl std::fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env(name) => write!(f, "env:{name}"),
            Self::Store(item) => write!(f, "store:{item}"),
            Self::File(path) => write!(f, "file:{}", path.display()),
        }
    }
}

impl Serialize for CredentialRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CredentialRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn valid_item(item: &str) -> bool {
    !item.is_empty()
        && item.len() <= 64
        && item
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && item.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

fn expand_tilde(path: &str) -> Result<PathBuf, String> {
    if path == "~" {
        return crate::utils::home_dir().map_err(|error| error.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return crate::utils::home_dir()
            .map(|home| home.join(rest))
            .map_err(|error| error.to_string());
    }
    Ok(PathBuf::from(path))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Credential {
    pub secret: Secret,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialError {
    Missing {
        reference: CredentialRef,
    },
    Empty {
        reference: CredentialRef,
    },
    Expired {
        reference: CredentialRef,
        at: u64,
    },
    Refused {
        reference: CredentialRef,
        why: String,
    },
    Store {
        reference: CredentialRef,
        why: String,
    },
    File {
        reference: CredentialRef,
        why: String,
    },
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { reference } => write!(f, "credential {reference} is missing"),
            Self::Empty { reference } => write!(f, "credential {reference} is empty"),
            Self::Expired { reference, at } => {
                write!(f, "credential {reference} expired at unix second {at}")
            }
            Self::Refused { reference, why } => {
                write!(f, "credential {reference} was refused: {why}")
            }
            Self::Store { reference, why } => write!(f, "credential {reference}: {why}"),
            Self::File { reference, why } => write!(f, "credential {reference}: {why}"),
        }
    }
}

impl std::error::Error for CredentialError {}

pub fn resolve(
    reference: &CredentialRef,
    env: EnvLookup<'_>,
    store: &dyn CredentialStore,
    now: u64,
) -> Result<Credential, CredentialError> {
    refuse_harness_login(reference)?;
    let value = match reference {
        CredentialRef::Env(name) => env(name).ok_or_else(|| CredentialError::Missing {
            reference: reference.clone(),
        })?,
        CredentialRef::Store(item) => store
            .get(item)
            .map_err(|why| CredentialError::Store {
                reference: reference.clone(),
                why,
            })?
            .ok_or_else(|| CredentialError::Missing {
                reference: reference.clone(),
            })?,
        CredentialRef::File(path) => {
            read_secret_file(path).map_err(|why| CredentialError::File {
                reference: reference.clone(),
                why,
            })?
        }
    };
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(CredentialError::Empty {
            reference: reference.clone(),
        });
    }
    let credential = Credential {
        secret: Secret::new(value),
        expires_at: None,
    };
    if let Some(at) = credential.expires_at
        && at < now
    {
        return Err(CredentialError::Expired {
            reference: reference.clone(),
            at,
        });
    }
    Ok(credential)
}

fn refuse_harness_login(reference: &CredentialRef) -> Result<(), CredentialError> {
    let refused = match reference {
        CredentialRef::Store(item) => item == "Claude Code-credentials",
        CredentialRef::File(path) => {
            let normalized = path.to_string_lossy().replace('\\', "/");
            normalized.ends_with(".claude/.credentials.json")
                || normalized.ends_with(".codex/auth.json")
        }
        CredentialRef::Env(_) => false,
    };
    if refused {
        Err(CredentialError::Refused {
            reference: reference.clone(),
            why: HARNESS_REFUSAL.to_string(),
        })
    } else {
        Ok(())
    }
}

fn read_secret_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "{} is group/world readable; run chmod 600 on it",
                path.display()
            ));
        }
    }
    std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))
}

pub trait CredentialStore {
    fn get(&self, item: &str) -> Result<Option<String>, String>;
    fn set(&self, item: &str, secret: &str) -> Result<(), String>;
}

#[cfg(test)]
#[derive(Default)]
pub struct FakeStore {
    values: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
}

#[cfg(test)]
impl FakeStore {
    pub fn with(item: &str, value: &str) -> Self {
        let mut values = std::collections::BTreeMap::new();
        values.insert(item.to_string(), value.to_string());
        Self {
            values: std::sync::Mutex::new(values),
        }
    }
}

#[cfg(test)]
impl CredentialStore for FakeStore {
    fn get(&self, item: &str) -> Result<Option<String>, String> {
        Ok(self
            .values
            .lock()
            .map_err(|e| e.to_string())?
            .get(item)
            .cloned())
    }

    fn set(&self, item: &str, secret: &str) -> Result<(), String> {
        self.values
            .lock()
            .map_err(|e| e.to_string())?
            .insert(item.to_string(), secret.to_string());
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: &'static str,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub inherit_stdin: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    NotFound,
    Timeout,
    Other(String),
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, command: &CommandSpec, timeout: Duration) -> Result<CommandOutput, RunError>;
}

struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&self, spec: &CommandSpec, timeout: Duration) -> Result<CommandOutput, RunError> {
        let mut command = Command::new(spec.program);
        command
            .args(&spec.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if spec.inherit_stdin {
            command.stdin(Stdio::inherit());
        } else {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                RunError::NotFound
            } else {
                RunError::Other(error.to_string())
            }
        })?;
        if let Some(input) = spec.stdin.as_deref()
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin
                .write_all(input.as_bytes())
                .map_err(|error| RunError::Other(error.to_string()))?;
        }
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let output = child
                        .wait_with_output()
                        .map_err(|error| RunError::Other(error.to_string()))?;
                    return Ok(CommandOutput {
                        status: output.status.code().unwrap_or(1),
                        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    });
                }
                Ok(None) if started.elapsed() >= timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunError::Timeout);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(error) => return Err(RunError::Other(error.to_string())),
            }
        }
    }
}

pub struct OsStore {
    runner: Box<dyn CommandRunner>,
    interactive: bool,
}

impl Default for OsStore {
    fn default() -> Self {
        Self {
            runner: Box::new(ProcessRunner),
            interactive: std::io::stdin().is_terminal(),
        }
    }
}

impl OsStore {
    #[cfg(test)]
    fn with_runner(runner: Box<dyn CommandRunner>, interactive: bool) -> Self {
        Self {
            runner,
            interactive,
        }
    }

    fn run(&self, spec: CommandSpec) -> Result<CommandOutput, String> {
        self.runner
            .run(&spec, Duration::from_secs(STORE_TIMEOUT_SECS))
            .map_err(|error| match error {
                RunError::NotFound => {
                    format!("{} not found; install it or use env:/file:", spec.program)
                }
                RunError::Timeout => {
                    format!("{} timed out after {STORE_TIMEOUT_SECS}s", spec.program)
                }
                RunError::Other(why) => format!("{} failed: {why}", spec.program),
            })
    }
}

impl CredentialStore for OsStore {
    fn get(&self, item: &str) -> Result<Option<String>, String> {
        let output = self.run(current_get_command(item))?;
        if output.status != 0 {
            return Ok(None);
        }
        Ok(Some(output.stdout.trim().to_string()))
    }

    fn set(&self, item: &str, secret: &str) -> Result<(), String> {
        let output = self.run(current_set_command(item, secret, self.interactive))?;
        if output.status == 0 {
            Ok(())
        } else {
            Err(format!(
                "credential store rejected the write (exit {})",
                output.status
            ))
        }
    }
}

#[cfg(target_os = "macos")]
fn current_get_command(item: &str) -> CommandSpec {
    macos_get_command(item)
}
#[cfg(target_os = "macos")]
fn current_set_command(item: &str, secret: &str, interactive: bool) -> CommandSpec {
    macos_set_command(item, secret, interactive)
}
#[cfg(target_os = "linux")]
fn current_get_command(item: &str) -> CommandSpec {
    linux_get_command(item)
}
#[cfg(target_os = "linux")]
fn current_set_command(item: &str, secret: &str, _interactive: bool) -> CommandSpec {
    linux_set_command(item, secret)
}
#[cfg(target_os = "windows")]
fn current_get_command(item: &str) -> CommandSpec {
    windows_get_command(item)
}
#[cfg(target_os = "windows")]
fn current_set_command(item: &str, secret: &str, _interactive: bool) -> CommandSpec {
    windows_set_command(item, secret)
}
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn current_get_command(_item: &str) -> CommandSpec {
    CommandSpec {
        program: "unsupported",
        args: vec![],
        stdin: None,
        inherit_stdin: false,
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn current_set_command(_item: &str, _secret: &str, _interactive: bool) -> CommandSpec {
    current_get_command("")
}

#[cfg(any(test, target_os = "macos"))]
pub fn macos_get_command(item: &str) -> CommandSpec {
    CommandSpec {
        program: "security",
        args: vec![
            "find-generic-password".into(),
            "-s".into(),
            "zirv-native".into(),
            "-a".into(),
            item.into(),
            "-w".into(),
        ],
        stdin: None,
        inherit_stdin: false,
    }
}

#[cfg(any(test, target_os = "macos"))]
pub fn macos_set_command(item: &str, secret: &str, interactive: bool) -> CommandSpec {
    let mut args = vec![
        "add-generic-password".into(),
        "-U".into(),
        "-s".into(),
        "zirv-native".into(),
        "-a".into(),
        item.into(),
        "-w".into(),
    ];
    if !interactive {
        args.push(secret.into());
    }
    CommandSpec {
        program: "security",
        args,
        stdin: None,
        inherit_stdin: interactive,
    }
}

#[cfg(any(test, target_os = "linux"))]
pub fn linux_get_command(item: &str) -> CommandSpec {
    CommandSpec {
        program: "secret-tool",
        args: vec![
            "lookup".into(),
            "service".into(),
            "zirv-native".into(),
            "item".into(),
            item.into(),
        ],
        stdin: None,
        inherit_stdin: false,
    }
}

#[cfg(any(test, target_os = "linux"))]
pub fn linux_set_command(item: &str, secret: &str) -> CommandSpec {
    CommandSpec {
        program: "secret-tool",
        args: vec![
            "store".into(),
            "--label".into(),
            format!("zirv native {item}"),
            "service".into(),
            "zirv-native".into(),
            "item".into(),
            item.into(),
        ],
        stdin: Some(secret.to_string()),
        inherit_stdin: false,
    }
}

#[cfg(any(test, target_os = "windows"))]
const WINDOWS_READ_SCRIPT: &str = r#"$p=Join-Path $env:LOCALAPPDATA ('zirv\native-credentials\'+$args[0]+'.dpapi'); if(!(Test-Path $p)){exit 3}; $b=[IO.File]::ReadAllBytes($p); $d=[Security.Cryptography.ProtectedData]::Unprotect($b,$null,'CurrentUser'); [Text.Encoding]::UTF8.GetString($d)"#;
#[cfg(any(test, target_os = "windows"))]
const WINDOWS_WRITE_SCRIPT: &str = r#"$d=Join-Path $env:LOCALAPPDATA 'zirv\native-credentials'; [IO.Directory]::CreateDirectory($d)|Out-Null; $s=[Console]::In.ReadToEnd(); $b=[Text.Encoding]::UTF8.GetBytes($s); $e=[Security.Cryptography.ProtectedData]::Protect($b,$null,'CurrentUser'); [IO.File]::WriteAllBytes((Join-Path $d ($args[0]+'.dpapi')),$e)"#;

#[cfg(any(test, target_os = "windows"))]
pub fn windows_get_command(item: &str) -> CommandSpec {
    CommandSpec {
        program: "powershell",
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            WINDOWS_READ_SCRIPT.into(),
            item.into(),
        ],
        stdin: None,
        inherit_stdin: false,
    }
}

#[cfg(any(test, target_os = "windows"))]
pub fn windows_set_command(item: &str, secret: &str) -> CommandSpec {
    CommandSpec {
        program: "powershell",
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            WINDOWS_WRITE_SCRIPT.into(),
            item.into(),
        ],
        stdin: Some(secret.to_string()),
        inherit_stdin: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_and_display_are_redacted() {
        let secret = Secret::new("sk-test-secret-123".into());
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(secret.to_string(), "[redacted]");
        assert_eq!(secret.expose(), "sk-test-secret-123");
    }

    #[test]
    fn missing_env_names_only_the_reference() {
        let reference: CredentialRef = "env:MISSING_KEY".parse().unwrap();
        let error = resolve(&reference, &|_| None, &FakeStore::default(), 10).unwrap_err();
        assert_eq!(error.to_string(), "credential env:MISSING_KEY is missing");
    }

    #[test]
    fn fake_store_returns_the_value_for_deterministic_resolution() {
        let store = FakeStore::with("work", "stored-key");
        let reference: CredentialRef = "store:work".parse().unwrap();
        assert_eq!(
            resolve(&reference, &|_| None, &store, 0)
                .unwrap()
                .secret
                .expose(),
            "stored-key"
        );
    }

    #[test]
    fn expired_error_names_time_and_reference_without_a_value() {
        let error = CredentialError::Expired {
            reference: "store:work".parse().unwrap(),
            at: 9,
        };
        assert_eq!(
            error.to_string(),
            "credential store:work expired at unix second 9"
        );
    }

    #[test]
    fn harness_login_refs_are_refused() {
        let refs = [
            "store:Claude Code-credentials",
            "file:/tmp/.claude/.credentials.json",
            "file:/tmp/.codex/auth.json",
        ];
        for raw in refs {
            let reference: CredentialRef = raw.parse().unwrap();
            let error = resolve(&reference, &|_| None, &FakeStore::default(), 0).unwrap_err();
            assert!(error.to_string().contains(HARNESS_REFUSAL), "got {error}");
        }
    }

    #[test]
    fn os_store_builders_keep_platform_specific_secret_handling() {
        assert_eq!(
            macos_get_command("work").args,
            [
                "find-generic-password",
                "-s",
                "zirv-native",
                "-a",
                "work",
                "-w"
            ]
        );
        let mac_prompt = macos_set_command("work", "secret", true);
        assert_eq!(
            mac_prompt.args,
            [
                "add-generic-password",
                "-U",
                "-s",
                "zirv-native",
                "-a",
                "work",
                "-w"
            ]
        );
        assert!(mac_prompt.inherit_stdin);
        assert!(
            macos_set_command("work", "secret", false)
                .args
                .ends_with(&["-w".into(), "secret".into()])
        );
        let linux = linux_set_command("work", "secret");
        assert_eq!(linux.program, "secret-tool");
        assert_eq!(
            linux_get_command("work").args,
            ["lookup", "service", "zirv-native", "item", "work"]
        );
        assert_eq!(
            linux.args,
            [
                "store",
                "--label",
                "zirv native work",
                "service",
                "zirv-native",
                "item",
                "work"
            ]
        );
        assert_eq!(linux.stdin.as_deref(), Some("secret"));
        assert!(!linux.args.contains(&"secret".into()));
        let windows = windows_set_command("work", "secret");
        assert_eq!(windows.program, "powershell");
        let windows_get = windows_get_command("work");
        assert_eq!(
            &windows_get.args[..3],
            ["-NoProfile", "-NonInteractive", "-Command"]
        );
        assert_eq!(windows_get.args.last().map(String::as_str), Some("work"));
        assert_eq!(
            &windows.args[..3],
            ["-NoProfile", "-NonInteractive", "-Command"]
        );
        assert_eq!(windows.args.last().map(String::as_str), Some("work"));
        assert_eq!(windows.stdin.as_deref(), Some("secret"));
        assert!(!windows.args.contains(&"secret".into()));
    }

    struct TimeoutRunner;

    impl CommandRunner for TimeoutRunner {
        fn run(&self, _: &CommandSpec, _: Duration) -> Result<CommandOutput, RunError> {
            Err(RunError::Timeout)
        }
    }

    #[test]
    fn os_store_reports_the_bounded_runner_timeout() {
        let store = OsStore::with_runner(Box::new(TimeoutRunner), false);
        let error = store.get("work").unwrap_err();
        assert!(error.contains("timed out after 3s"), "got {error}");
    }

    #[cfg(unix)]
    #[test]
    fn file_credentials_must_be_private_regular_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "value\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let reference = CredentialRef::File(path.clone());
        assert_eq!(
            resolve(&reference, &|_| None, &FakeStore::default(), 0)
                .unwrap()
                .secret
                .expose(),
            "value"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve(&reference, &|_| None, &FakeStore::default(), 0).is_err());
    }
}
