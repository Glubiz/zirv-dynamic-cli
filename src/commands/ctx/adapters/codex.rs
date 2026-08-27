use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::super::CtxResult;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, TranscriptUsage,
};
use super::super::window::{self, RolloutRecord};
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// Verified facts backing this adapter live in
/// `docs/superpowers/notes/2026-07-31-codex-cli-facts.md`. Current Codex
/// releases expose both lifecycle hooks and the external `notify` program;
/// `zirv setup` uses the documented lifecycle-hook schema.
///
/// `ready()` no longer hard-errors: codex is a supported adapter with an
/// honestly degraded capability set, while direct launches support developer
/// instructions. It is selectable and launchable in the common case (`codex`
/// resolves to a real binary) and also when nothing named `codex` is
/// installed at all -- `resolve_program` fails open for that case, so
/// `--agent codex` on a machine without it fails at spawn time with the OS's
/// own "not found", not here. The one launch `ready()` actually refuses is a
/// bare `codex` that resolves via `PATH` to a file this OS cannot execute at
/// all.
///
/// `parse_events`/`structural_context` (issue #86, 2026-08-23) derive real
/// normalized events from the same rollout JSONL `window.rs` already parses
/// for usage-window state, via the shared `window::parse_rollout_record`
/// collector -- see that function's own doc comment and `parse_events`
/// below for exactly which rollout shapes are verified and mapped. Tool
/// calls, tool results, compaction boundaries, and assistant text outside
/// `task_complete.last_agent_message` still have no verified rollout shape
/// (see `docs/superpowers/notes/2026-07-31-codex-cli-facts.md`) and are not
/// modeled -- tracked as the residual half of
/// [issue #11](https://github.com/Glubiz/zirv-dynamic-cli/issues/11).
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    /// Test seam only: forces `ignore_flags_supported`'s answer instead of
    /// spawning a real `--help` probe against whatever "codex" happens to
    /// resolve to on the machine running the test suite -- without this,
    /// `read_only_args`/`sandbox_residual_note` tests would be
    /// non-deterministic depending on whether a real codex-cli sits on the
    /// test machine's `PATH`. Mirrors `ClaudeAdapter`'s own
    /// `forced_file_support` field exactly.
    #[cfg(test)]
    forced_ignore_flags_support: Option<bool>,
    /// Test seam only, mirroring `forced_ignore_flags_support` exactly:
    /// forces `on_request_approval_supported`'s answer instead of spawning a
    /// real `--help` probe against whatever "codex" happens to resolve to on
    /// the machine running the test suite.
    #[cfg(test)]
    forced_on_request_approval_support: Option<bool>,
    /// Test seam for the current CLI's `--approve-for-me` automatic
    /// boundary reviewer. Kept separate from `on-request`: older builds may
    /// support the approval mode without supporting automatic review.
    #[cfg(test)]
    forced_auto_review_support: Option<bool>,
    /// Test seam for `exec_ask_for_approval_supported`'s answer (issue
    /// #134): forces whether the installed codex-cli's `codex exec --help`
    /// is treated as documenting `--ask-for-approval`, instead of spawning a
    /// real probe against whatever "codex" happens to resolve to on the
    /// machine running the test suite. Kept separate from
    /// `forced_on_request_approval_support`: that field forces the
    /// TOP-LEVEL `codex --help` probe (a different command surface, gating
    /// the interactive `on-request` value), while this one forces the
    /// `exec`-scoped probe that gates whether the flag itself may appear on
    /// a headless `codex exec` launch at all.
    #[cfg(test)]
    forced_exec_ask_for_approval_support: Option<bool>,
}

impl CodexAdapter {
    /// `bin` may carry arguments, so `"sh /tmp/stub.sh"` and
    /// `"/usr/bin/env codex"` both work, mirroring `ClaudeAdapter::new`.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("codex").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "codex".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_ignore_flags_support: None,
            #[cfg(test)]
            forced_on_request_approval_support: None,
            #[cfg(test)]
            forced_auto_review_support: None,
            #[cfg(test)]
            forced_exec_ask_for_approval_support: None,
        }
    }

    /// Test seam: pins the home directory the transcript path is built from.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Test seam: see the field's own doc comment.
    #[cfg(test)]
    pub fn with_ignore_flags_forced(mut self, supported: bool) -> Self {
        self.forced_ignore_flags_support = Some(supported);
        self
    }

    /// Test seam: see the field's own doc comment.
    #[cfg(test)]
    pub fn with_on_request_approval_forced(mut self, supported: bool) -> Self {
        self.forced_on_request_approval_support = Some(supported);
        self
    }

    /// Test seam: see the field's own doc comment.
    #[cfg(test)]
    pub fn with_auto_review_forced(mut self, supported: bool) -> Self {
        self.forced_auto_review_support = Some(supported);
        self
    }

    /// Test seam: see the field's own doc comment.
    #[cfg(test)]
    pub fn with_exec_ask_for_approval_forced(mut self, supported: bool) -> Self {
        self.forced_exec_ask_for_approval_support = Some(supported);
        self
    }

    /// Whether the installed codex-cli's own top-level `codex --help`
    /// documents the `on-request` value of `-a, --ask-for-approval`
    /// (2026-08-24, cross-harness permissions design).
    ///
    /// Probed, never assumed, for exactly the reason `ignore_flags_supported`
    /// above is: the real minimum supporting version is unknown, and passing
    /// a value an older install does not recognize is an
    /// unrecognized-argument error that breaks the launch outright. Fails
    /// closed (`false`) on any doubt at all: binary missing, timeout, or
    /// `--help` output that does not name both the flag and the value.
    ///
    /// The TOP-LEVEL `--help` is probed, not `exec --help`: this gates the
    /// INTERACTIVE launch (`codex [PROMPT]`, built by `interactive_cmd`),
    /// which is a different command surface from the headless `codex exec`
    /// that `ignore_flags_supported` probes.
    fn on_request_approval_supported(&self) -> bool {
        #[cfg(test)]
        if let Some(forced) = self.forced_on_request_approval_support {
            return forced;
        }
        probe_on_request_approval_support(&self.program, &self.bin_args)
    }

    /// Whether the installed top-level CLI documents `--approve-for-me`,
    /// which routes only sandbox-boundary approvals through Codex's own
    /// security reviewer. Unknown/older binaries retain plain `on-request`.
    fn auto_review_supported(&self) -> bool {
        #[cfg(test)]
        if let Some(forced) = self.forced_auto_review_support {
            return forced;
        }
        probe_auto_review_support(&self.program, &self.bin_args)
    }

    /// Issue #89: whether the installed codex-cli's own `codex exec --help`
    /// documents BOTH `--ignore-rules` and `--ignore-user-config` -- the two
    /// flags that would close the residual `sandbox_residual_note`
    /// describes. Probed rather than guessed from a hardcoded version
    /// string, the same `--help`-probe-over-version-cutoff choice
    /// `ClaudeAdapter::supports_system_prompt_file` already made, and for
    /// the identical reason: the real minimum supporting version is
    /// unknown (0.105.0, the npm-published build, has neither flag; 0.147.0
    /// has both -- nothing in between was ever captured), so a live probe
    /// is the only honest answer. Fails closed (`false`) on any doubt at
    /// all: binary missing, timeout, or `--help` output missing either
    /// flag -- see `distiller_cmd`'s own doc comment for why passing just
    /// one on an unsupporting install is worse than passing neither.
    fn ignore_flags_supported(&self) -> bool {
        #[cfg(test)]
        if let Some(forced) = self.forced_ignore_flags_support {
            return forced;
        }
        probe_ignore_flags_support(&self.program, &self.bin_args)
    }

    /// Issue #134: whether the installed codex-cli's own `codex exec --help`
    /// documents `--ask-for-approval` at all. Unlike
    /// `on_request_approval_supported` (which probes the TOP-LEVEL `codex
    /// --help` to decide whether the `on-request` VALUE is safe to pass on
    /// the interactive launch), this probes the `exec`-scoped help text --
    /// the different command surface the headless launch
    /// (`headless_cmd`/`headless_cmd_stdin`) actually uses -- to decide
    /// whether the FLAG ITSELF may be passed at all. Current codex-cli
    /// (0.149.x) accepts `--ask-for-approval` on the top-level interactive
    /// launch but rejects it as an unrecognized argument on `codex exec`
    /// (`error: unexpected argument '--ask-for-approval' found`), so a
    /// probe scoped to the wrong surface would wrongly conclude the headless
    /// launch is safe. Fails closed (`false`, i.e. "not supported") on any
    /// doubt at all -- binary missing, timeout, or `--help` output missing
    /// the flag -- exactly like every other probe in this module, since a
    /// false negative here only costs the config-override fallback
    /// (`-c approval_policy=never`, verified to work on both interactive and
    /// `exec` launches per `system_prompt_args`'s own doc comment), while a
    /// false positive would pass an argument the binary rejects and break
    /// the launch outright.
    fn exec_ask_for_approval_supported(&self) -> bool {
        #[cfg(test)]
        if let Some(forced) = self.forced_exec_ask_for_approval_support {
            return forced;
        }
        probe_exec_ask_for_approval_support(&self.program, &self.bin_args)
    }

    /// Issue #134: `codex exec --help` on current codex-cli (0.149.x) no
    /// longer documents `--ask-for-approval` at all -- passing it breaks a
    /// headless launch outright (`error: unexpected argument
    /// '--ask-for-approval' found`) even though the SAME flag is still
    /// accepted on the top-level interactive `codex` launch. Probed via
    /// `exec_ask_for_approval_supported` (an `exec`-scoped `--help` probe,
    /// deliberately separate from `on_request_approval_supported`'s
    /// top-level probe -- see that method's own doc comment for why the two
    /// surfaces can disagree), never assumed.
    ///
    /// On an interactive launch, or a headless launch whose installed CLI
    /// still documents the flag, this returns the plain `--ask-for-approval
    /// <value>` pair -- byte-for-byte what every caller emitted before this
    /// method existed, so an install that still supports the flag sees no
    /// change at all. Only when BOTH conditions hold (headless AND
    /// unsupported) does this fall back to `-c approval_policy=<value>`:
    /// `-c/--config key=value` overrides are documented on both the
    /// interactive and `exec` command surfaces (`system_prompt_args`'s own
    /// doc comment), and `approval_policy` is the config key codex's own
    /// `~/.codex/config.toml` schema uses for this exact setting (surfaced
    /// in `codex exec`'s own stdout preamble as `approval: <value>`,
    /// `policy_support`'s `CONFIG` constant), so this fallback carries the
    /// identical posture through a mechanism the binary still accepts
    /// rather than silently dropping the suppression.
    fn approval_suppression_args(&self, mode: super::LaunchMode, value: &str) -> Vec<String> {
        if !mode.is_interactive() && !self.exec_ask_for_approval_supported() {
            vec!["-c".to_string(), format!("approval_policy={value}")]
        } else {
            vec!["--ask-for-approval".to_string(), value.to_string()]
        }
    }

    /// Every command starts here so the program and its leading arguments are
    /// applied uniformly to headless, interactive and distiller invocations.
    ///
    /// SECURITY (FINDING 6, closed): this now mirrors `ClaudeAdapter::base`
    /// exactly -- the program is routed through `super::resolve_program` so an
    /// npm-installed `codex.cmd` shim launches at all on Windows, and
    /// `launches_through_cmd_shim` below reports the same shim shape so a
    /// caller moves the headless prompt onto stdin
    /// (`headless_cmd_stdin`) rather than argv on exactly that launch shape.
    /// There is no `system_prompt_file_flag` override because there is no
    /// verified per-run system-prompt mechanism at all for codex (see
    /// `system_prompt_args` below) -- nothing to force off argv, since
    /// nothing is ever put on it in the first place.
    fn base(&self) -> Command {
        let resolved = super::resolve_program(&self.program)
            .unwrap_or_else(|_| ResolvedProgram::direct(&self.program));
        let mut cmd = Command::new(&resolved.program);
        cmd.args(&resolved.prefix);
        cmd.args(&self.bin_args);
        cmd
    }

    fn home_dir(&self) -> PathBuf {
        self.home
            .clone()
            .or_else(|| crate::utils::home_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Bounds the `codex exec --help` probe below: a hang here must never hang
/// a distiller/reviewer spawn. Mirrors `claude.rs`'s own
/// `HELP_PROBE_TIMEOUT`.
const IGNORE_FLAGS_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Process-wide cache of `detect_ignore_flags`'s answer, keyed by the exact
/// program invocation -- mirrors `claude.rs`'s own `ProbeKey`/
/// `SYSTEM_PROMPT_FILE_SUPPORT` for the identical reason: `agent_bin` can
/// point at a different binary, or a different version resolved off a
/// different `PATH`, and each has its own answer.
type ProbeKey = (PathBuf, Vec<String>);
static IGNORE_FLAGS_SUPPORT: OnceLock<Mutex<HashMap<ProbeKey, bool>>> = OnceLock::new();

fn probe_ignore_flags_support(program: &str, bin_args: &[String]) -> bool {
    let key = (PathBuf::from(program), bin_args.to_vec());
    let cache = IGNORE_FLAGS_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = cache.lock() else {
        return false;
    };
    if let Some(cached) = map.get(&key) {
        return *cached;
    }
    let detected = detect_ignore_flags(program, bin_args);
    map.insert(key, detected);
    detected
}

/// Runs `<program> [bin_args] exec --help` and reports whether its output
/// names BOTH `--ignore-rules` and `--ignore-user-config`. Verified against
/// the real installed `codex-cli 0.147.0`'s own `codex exec --help`
/// (2026-08-23), which documents both; the npm-published `0.105.0` most
/// operators get documents neither (see `distiller_cmd`'s own doc comment).
/// Any doubt at all -- binary missing, timeout, output missing either flag
/// -- reads as unsupported: passing just one on an install that does not
/// recognize it is very likely an unrecognized-argument error that breaks
/// the distiller/reviewer outright, worse than the residual these flags
/// exist to close.
fn detect_ignore_flags(program: &str, bin_args: &[String]) -> bool {
    // The same resolution the real launch uses, exactly like claude's own
    // `detect_help_flag` -- otherwise the probe and the spawn could disagree
    // on Windows about whether this is a `.cmd` shim.
    let resolved =
        super::resolve_program(program).unwrap_or_else(|_| ResolvedProgram::direct(program));

    // SECURITY: identical defense to `claude.rs::detect_help_flag` -- run
    // the fail-closed reparse guard against the exact probe argv before
    // spawning, since `bin_args` can carry repo-controlled text on some
    // launch shapes and a shim-resolved probe would otherwise let cmd.exe
    // reparse it before the real launch's own guard is ever reached.
    let mut probe_args: Vec<String> =
        Vec::with_capacity(resolved.prefix.len() + bin_args.len() + 2);
    probe_args.extend(resolved.prefix.iter().cloned());
    probe_args.extend(bin_args.iter().cloned());
    probe_args.push("exec".to_string());
    probe_args.push("--help".to_string());
    if super::guard_cmd_shim_reparse(&resolved.program, &probe_args).is_err() {
        return false;
    }

    let Ok(mut child) = Command::new(&resolved.program)
        .args(&resolved.prefix)
        .args(bin_args)
        .arg("exec")
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let mut stdout_pipe = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = stdout_pipe.take() {
            let _ = pipe.read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + IGNORE_FLAGS_PROBE_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
            return text.contains("--ignore-rules") && text.contains("--ignore-user-config");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Process-wide cache of `detect_exec_ask_for_approval_support`'s answer,
/// keyed exactly like [`IGNORE_FLAGS_SUPPORT`] and for the identical reason.
static EXEC_ASK_FOR_APPROVAL_SUPPORT: OnceLock<Mutex<HashMap<ProbeKey, bool>>> = OnceLock::new();

fn probe_exec_ask_for_approval_support(program: &str, bin_args: &[String]) -> bool {
    let key = (PathBuf::from(program), bin_args.to_vec());
    let cache = EXEC_ASK_FOR_APPROVAL_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = cache.lock() else {
        return false;
    };
    if let Some(cached) = map.get(&key) {
        return *cached;
    }
    let detected = detect_exec_ask_for_approval_support(program, bin_args);
    map.insert(key, detected);
    detected
}

/// Runs `<program> [bin_args] exec --help` and reports whether its output
/// names `--ask-for-approval`. Mirrors `detect_ignore_flags` exactly (same
/// resolution, same fail-closed cmd-shim reparse guard, same timeout shape),
/// differing only in the marker it looks for -- issue #134.
fn detect_exec_ask_for_approval_support(program: &str, bin_args: &[String]) -> bool {
    let resolved =
        super::resolve_program(program).unwrap_or_else(|_| ResolvedProgram::direct(program));

    let mut probe_args: Vec<String> =
        Vec::with_capacity(resolved.prefix.len() + bin_args.len() + 2);
    probe_args.extend(resolved.prefix.iter().cloned());
    probe_args.extend(bin_args.iter().cloned());
    probe_args.push("exec".to_string());
    probe_args.push("--help".to_string());
    if super::guard_cmd_shim_reparse(&resolved.program, &probe_args).is_err() {
        return false;
    }

    let Ok(mut child) = Command::new(&resolved.program)
        .args(&resolved.prefix)
        .args(bin_args)
        .arg("exec")
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let mut stdout_pipe = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = stdout_pipe.take() {
            let _ = pipe.read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + IGNORE_FLAGS_PROBE_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
            return text.contains("--ask-for-approval");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Bounds the top-level `codex --help` probe below, exactly as
/// [`IGNORE_FLAGS_PROBE_TIMEOUT`] bounds the `codex exec --help` one.
const ON_REQUEST_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Process-wide cache of `detect_on_request_approval`'s answer, keyed by the
/// exact program invocation -- the identical `ProbeKey` shape
/// [`IGNORE_FLAGS_SUPPORT`] uses, for the identical reason: `agent_bin` can
/// point at a different binary, or a different version resolved off a
/// different `PATH`, and each has its own answer.
///
/// KNOWN LIMITATION (2026-08-24, filed rather than guessed at): the cache
/// is keyed on `(program, bin_args)`, not on the resolved binary's mtime or
/// version string, so it never invalidates if the SAME path is upgraded or
/// downgraded mid-process (a codex-cli reinstall while a long-lived `zirv
/// ctx` session, dashboard, or supervisor keeps running). A stale cached
/// `true` after a downgrade that dropped `--approve-for-me`/on-request
/// approval support would pass an unsupported flag; a stale cached `false`
/// after an upgrade that added it merely withholds a capability, the safe
/// direction. Bounding this needs a cache-busting signal (mtime/version)
/// this module does not currently probe for; out of scope for this pass.
static ON_REQUEST_APPROVAL_SUPPORT: OnceLock<Mutex<HashMap<ProbeKey, bool>>> = OnceLock::new();
static AUTO_REVIEW_SUPPORT: OnceLock<Mutex<HashMap<ProbeKey, bool>>> = OnceLock::new();

fn probe_on_request_approval_support(program: &str, bin_args: &[String]) -> bool {
    let key = (PathBuf::from(program), bin_args.to_vec());
    let cache = ON_REQUEST_APPROVAL_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = cache.lock() else {
        return false;
    };
    if let Some(cached) = map.get(&key) {
        return *cached;
    }
    let detected = detect_on_request_approval(program, bin_args);
    map.insert(key, detected);
    detected
}

fn probe_auto_review_support(program: &str, bin_args: &[String]) -> bool {
    let key = (PathBuf::from(program), bin_args.to_vec());
    let cache = AUTO_REVIEW_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = cache.lock() else {
        return false;
    };
    if let Some(cached) = map.get(&key) {
        return *cached;
    }
    let detected = detect_top_level_help_marker(program, bin_args, "--approve-for-me");
    map.insert(key, detected);
    detected
}

/// Runs `<program> [bin_args] --help` and reports whether its output names
/// BOTH `--ask-for-approval` and the `on-request` value. Any doubt at all --
/// binary missing, timeout, output missing either string -- reads as
/// unsupported, and the caller keeps `never`.
fn detect_on_request_approval(program: &str, bin_args: &[String]) -> bool {
    detect_top_level_help_markers(program, bin_args, &["--ask-for-approval", "on-request"])
}

fn detect_top_level_help_marker(program: &str, bin_args: &[String], marker: &str) -> bool {
    detect_top_level_help_markers(program, bin_args, &[marker])
}

/// Runs one bounded top-level help probe and requires every named marker.
/// This is shared by the approval-mode and automatic-review capability
/// checks so their resolution, cmd-shim guard, timeout and failure polarity
/// cannot drift.
fn detect_top_level_help_markers(program: &str, bin_args: &[String], markers: &[&str]) -> bool {
    // The same resolution the real launch uses, exactly like
    // `detect_ignore_flags` -- otherwise the probe and the spawn could
    // disagree on Windows about whether this is a `.cmd` shim.
    let resolved =
        super::resolve_program(program).unwrap_or_else(|_| ResolvedProgram::direct(program));

    // SECURITY: identical defense to `detect_ignore_flags` -- run the
    // fail-closed reparse guard against the exact probe argv before spawning,
    // since `bin_args` can carry repo-controlled text on some launch shapes.
    let mut probe_args: Vec<String> =
        Vec::with_capacity(resolved.prefix.len() + bin_args.len() + 1);
    probe_args.extend(resolved.prefix.iter().cloned());
    probe_args.extend(bin_args.iter().cloned());
    probe_args.push("--help".to_string());
    if super::guard_cmd_shim_reparse(&resolved.program, &probe_args).is_err() {
        return false;
    }

    let Ok(mut child) = Command::new(&resolved.program)
        .args(&resolved.prefix)
        .args(bin_args)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let mut stdout_pipe = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = stdout_pipe.take() {
            let _ = pipe.read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + ON_REQUEST_PROBE_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
            return markers.iter().all(|marker| text.contains(marker));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Codex nests rollout files under `<sessions>/<YYYY>/<MM>/<DD>/`, a depth
/// `SessionRef` cannot predict (it carries only the session id, not the
/// session's start time), so the transcript is found by filename suffix
/// rather than a computed path.
fn find_rollout(dir: &Path, filename_suffix: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_rollout(&path, filename_suffix) {
                return Some(found);
            }
        } else if path.to_string_lossy().ends_with(filename_suffix) {
            return Some(path);
        }
    }
    None
}

/// `value` rendered as a quoted TOML string, for embedding inside a `-c
/// key=["..."]`-style config-override argv token (`extra_writable_root_
/// args`, `approval_suppression_args`'s sibling for path values rather than
/// bare identifiers). codex's own `-c`/`--config` value parser is TOML, so
/// this always produces syntax any TOML parser accepts, never a
/// zirv-invented format.
///
/// **Prefers a TOML LITERAL string (`'...'`)** -- no escape processing at
/// all, so a Windows path's `\` survives untouched -- **over a basic string
/// (`"..."`), specifically to avoid ever emitting a literal `"`.** A real
/// bug (2026-08-26, found running this branch's own gates on Windows): the
/// previous basic-string form escaped `\`/`"` correctly per TOML's own
/// rules, but `guard_cmd_shim_reparse`'s `CMD_REPARSE_METACHARS` refuses any
/// argument containing a raw `"` outright on a `cmd.exe`/`powershell` shim
/// launch (CVE-2024-24576's quote-toggle) -- so the moment `extra_writable_
/// root_args` was wired in (unconditionally, into every dashboard-spawned
/// worker pane), a codex pane reached through an npm-installed `.cmd` or a
/// `.ps1` could never launch at all: its own writable-roots argument tripped
/// the very guard meant to stop injected content, despite carrying nothing
/// but zirv-computed paths. A literal string sidesteps this rather than
/// weakening the guard, which stays a correct, unconditional backstop.
///
/// Falls back to an escaped basic string only when `value` itself contains a
/// literal `'`, which a TOML literal string has no way to represent at all
/// (pathological for a real filesystem path; that case is not, and was never,
/// spawnable through a shim launch either way).
fn toml_quoted_string(value: &str) -> String {
    if !value.contains('\'') && !value.chars().any(|c| c.is_control() && c != '\t') {
        return format!("'{value}'");
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

impl AgentAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// Codex spends an OpenAI account's limits. Nothing collects readings for
    /// it yet, which is exactly why the provider is named: a usage readout
    /// can then say "openai: no usage source" rather than imply zero.
    fn provider(&self) -> &'static str {
        "openai"
    }

    /// Mirrors `ClaudeAdapter::ready` exactly: the one thing that can make
    /// this adapter unusable before it is asked to do anything is a bare
    /// name that *does* resolve (via `PATH`) to a file this OS has no way to
    /// execute. `resolve_program` fails open for the opposite case -- a name
    /// that resolves to nothing at all -- so `--agent codex` succeeds even
    /// when `codex` is not installed anywhere; that case is left to surface
    /// as the OS's own "not found" at spawn time, not caught here. Codex
    /// support is otherwise honestly degraded (see the module doc comment)
    /// but not refused.
    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "codex")
            .unwrap_or(false)
    }

    /// `codex exec` has no `--session-id` flag (verified): codex always mints
    /// its own session id, so `session` cannot appear in the built command.
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec").arg(prompt).args(extra);
        cmd
    }

    /// True on a Windows npm install, where `codex` is a `.cmd` shim that
    /// [`super::resolve_program`] routes through `cmd.exe /c`. Exactly
    /// `ClaudeAdapter::launches_through_cmd_shim`'s own body: that is the one
    /// launch shape where a headless prompt on argv would be reparsed by
    /// cmd.exe, so on it the prompt is delivered via stdin instead
    /// (`headless_cmd_stdin`).
    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// `codex exec [PROMPT]` reads its prompt from stdin when the positional
    /// argument is omitted (verified:
    /// docs/superpowers/notes/2026-07-31-codex-cli-facts.md, line 8 -- "If
    /// `[PROMPT]` is omitted (or is `-`), instructions are read from stdin"),
    /// so the same `exec` invocation `headless_cmd` builds works here with
    /// just the positional prompt token dropped. This is what lets
    /// `launches_through_cmd_shim` above move a headless prompt off argv on a
    /// Windows `.cmd` shim launch, exactly like claude's own stdin form.
    fn headless_cmd_stdin(&self, _session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("exec").args(extra);
        Some(cmd)
    }

    /// With no subcommand, `codex [PROMPT]` forwards straight to the
    /// interactive CLI (verified via `codex --help`), exactly like claude.
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        // Current Codex exposes `-c/--config key=value` on interactive and
        // `exec` launches, and its public config schema defines
        // `developer_instructions` as a developer-role message. JSON string
        // encoding is also valid TOML basic-string encoding for this value.
        let value = serde_json::to_string(prompt).expect("serializing a Rust string cannot fail");
        vec!["-c".to_string(), format!("developer_instructions={value}")]
    }

    /// Spelled out rather than left to the trait default: the only base layer zirv has is written
    /// around Claude Code's tools (the Agent tool, `.claude/agents`, the
    /// `/code-review` skill), none of which codex has. Instructions about
    /// tools an agent does not have are worse than no instructions.
    fn base_system_prompt(&self) -> Option<&'static str> {
        None
    }

    /// Verified via `codex exec --help` (quoted verbatim in the notes file):
    /// `-m, --model <MODEL>` is a real flag, and the prompt is read from
    /// stdin when none is given as an argument, so the distillation prompt
    /// never hits an argv length limit. `model` is empty when neither the
    /// operator's own config nor `default_distiller_model` (`None` for
    /// codex, since zirv has no verified cheap model name for this lineup)
    /// named one -- omitting `--model` entirely rather than passing an empty
    /// value lets codex's own `~/.codex/config.toml` default apply instead
    /// of zirv guessing a model name that may not exist on the operator's
    /// account.
    ///
    /// `--sandbox read-only` is codex's own analogue of claude's
    /// `--disallowedTools=Write,Edit,Bash,NotebookEdit` pin: it is what backs
    /// `zirv ctx optimize`'s report-only guarantee for a codex judgment
    /// child. Verified against the real installed CLI (`codex exec --help`,
    /// codex-cli 0.105.0): `-s, --sandbox <SANDBOX_MODE>`, possible values
    /// `read-only`/`workspace-write`/`danger-full-access`. It blocks the
    /// class of risk claude's own restriction was verified to close (this
    /// child writing a file, running a shell command, or otherwise mutating
    /// the checkout via a tool) -- but it is not the identical guarantee:
    /// `--sandbox` restricts what codex-*executed shell commands* may touch,
    /// not which of codex's own tools may run at all, and codex's own
    /// AGENTS.md (this repo's equivalent of CLAUDE.md, read into context the
    /// same way) is still embedded in this child's prompt just like claude's
    /// distiller embeds CLAUDE.md -- read-only scopes what the sandbox lets
    /// an executed command do, it does not stop the model from reading that
    /// text or from answering based on it.
    ///
    /// ISSUE #89 UPDATE (2026-08-23): `read_only_args` below now adds
    /// `--ignore-rules --ignore-user-config` automatically, but only once a
    /// live `--help` probe (`ignore_flags_supported`) confirms the installed
    /// codex-cli actually documents both. codex-cli 0.146.0's `codex exec
    /// --help` (the brew-installed capture in `docs/superpowers/notes/
    /// 2026-07-31-codex-cli-facts.md`) documents both; the npm-published
    /// `0.105.0` most operators get documents neither, and passing either on
    /// an install that does not recognize it would very likely be an
    /// unrecognized-argument error, breaking every distiller call for that
    /// install rather than sandboxing it further -- which is exactly why
    /// this is probed live rather than gated on a hardcoded version cutoff
    /// (see `ignore_flags_supported`'s own doc comment). On an install
    /// where the probe says "no", the operator's own `.rules` execpolicy
    /// files and `~/.codex/config.toml` still shape this judgment child's
    /// behavior (unlike claude's distiller, whose CLAUDE.md-reading is the
    /// one thing `--disallowedTools` cannot touch either, so the two
    /// residuals are not symmetric: claude's is "still reads the file",
    /// codex's un-upgraded residual is "still reads the file *and* still
    /// honors config it did not ask for") -- `sandbox_residual_note` names
    /// this for the operator via a one-time `zirv ▸` announcement.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec");
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }
        cmd.args(self.read_only_args());
        cmd
    }

    /// `--sandbox read-only` plus, when `ignore_flags_supported()` confirms
    /// the installed codex-cli documents them, `--ignore-rules
    /// --ignore-user-config` (issue #89). Shared by `distiller_cmd` above
    /// and the workflow reviewer (`workflow::review::reviewer_argv`, via
    /// `adapters::read_only_args_for_agent_name`), so both consumers of
    /// this pin get the stronger guarantee together, on the same installed
    /// binary's own verified capability, rather than drifting apart.
    fn read_only_args(&self) -> Vec<String> {
        let mut args = vec!["--sandbox".to_string(), "read-only".to_string()];
        if self.ignore_flags_supported() {
            args.push("--ignore-rules".to_string());
            args.push("--ignore-user-config".to_string());
        }
        args
    }

    /// Issue #89: names the residual for the operator when the installed
    /// codex-cli's `codex exec --help` does not document `--ignore-rules`/
    /// `--ignore-user-config` (see `read_only_args`/`ignore_flags_supported`
    /// above) -- `None` once it does, which is what stops
    /// `adapters::announce_sandbox_residual_once` from firing for an
    /// operator on a newer build.
    fn sandbox_residual_note(&self) -> Option<String> {
        if self.ignore_flags_supported() {
            return None;
        }
        Some(
            "codex's report-only sandbox (--sandbox read-only) could not add --ignore-rules \
             --ignore-user-config on this installed codex-cli, so the distiller/reviewer child \
             still reads this repo's .rules execpolicy files and your ~/.codex/config.toml on \
             top of AGENTS.md. Upgrade codex-cli to a version whose `codex exec --help` \
             documents both flags to close this automatically."
                .to_string(),
        )
    }

    /// Codex's own model ladder, top to bottom: `gpt-5.6-sol` (the default
    /// used when no `-m` is given), `gpt-5.6-terra`, `gpt-5.6-luna`, and the
    /// older, hidden `gpt-5.4-mini` -- verified via `codex debug models` in
    /// docs/superpowers/notes/2026-07-31-codex-cli-facts.md's "Cheap model
    /// alias for distillation" section (codex-cli 0.146.0). Matched by
    /// substring on `seat`, lowercased first (same as claude's own ladder)
    /// so a mixed-case seat still lands on the right rung instead of
    /// falling through to the unknown arm. `gpt-5.4-mini` is already the
    /// floor, so it maps to itself; an absent or unrecognised seat
    /// (including one naming another adapter's model, e.g. a claude
    /// orchestrator's own `chat.model`) assumes the top tier -- the
    /// deliberate consequence is that the computed default can then resolve
    /// to a model *more expensive* than the seat actually in use (an
    /// accepted spend-up default; the operator can override it with
    /// `[review]` or by setting `chat.model`).
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        let seat = seat.map(str::to_lowercase);
        match seat.as_deref() {
            Some(s) if s.contains("gpt-5.6-sol") => "gpt-5.6-terra",
            Some(s) if s.contains("gpt-5.6-terra") => "gpt-5.6-luna",
            Some(s) if s.contains("gpt-5.6-luna") => "gpt-5.4-mini",
            Some(s) if s.contains("gpt-5.4-mini") => "gpt-5.4-mini",
            _ => "gpt-5.6-terra",
        }
    }

    /// Codex's descriptors come from the repo's **recorded** facts
    /// (docs/superpowers/notes/2026-07-31-codex-cli-facts.md, and the
    /// `distiller_cmd` notes above), not from a live CLI: codex is not
    /// runnable on the machine this was written on, so anything not in those
    /// notes is reported unsupported rather than guessed at.
    ///
    /// `-s, --sandbox <read-only|workspace-write|danger-full-access>` is the
    /// one verified enforcement flag, and it scopes what a codex-*executed
    /// shell command may write*, not whether shell commands may execute or
    /// which of codex's own tools may run (see `docs/obsidian/Concepts/
    /// Untrusted Configuration.md`). That distinction matters per capability:
    ///
    /// - **Repo filesystem writes** are `Degraded` at `Deny`: read-only really
    ///   does block writes inside the repo, just not by denying any tool.
    /// - **Writes outside the repo** are `Degraded` at `Deny` the same way,
    ///   via `--sandbox workspace-write` (writes confined to the workspace).
    ///
    ///   **These two descriptors name mechanisms that conflict, not merely
    ///   differ.** `--sandbox` is a single flag with one value per launch:
    ///   `read-only` cannot be passed alongside `workspace-write` on the same
    ///   command line. So a policy asking to deny `repo_fs_write` *and* deny
    ///   `outside_repo_fs_write` in the same launch cannot have both
    ///   descriptors' mechanisms honored at once -- issue #44's pin
    ///   construction, which turns an `EffectivePolicy` into actual argv,
    ///   has to resolve that conflict itself (e.g. `read-only` is the
    ///   stricter of the two and implies the other, so it should win), not
    ///   assume each capability's mechanism composes independently the way
    ///   claude's tool-name pins do.
    /// - **Shell execution** is `Unsupported` at `Deny`: read-only scopes what
    ///   a command may write, it does not stop a command from running at
    ///   all. A worker under `--sandbox read-only` can still run, say,
    ///   `cat ~/.aws/credentials` -- reading is untouched by a write-scoping
    ///   flag, so claiming `Degraded` here would overstate what the sandbox
    ///   does.
    /// - **Approval** is `Degraded` at `Deny` (2026-08-22, revised from
    ///   `Unsupported`): `-a, --ask-for-approval <untrusted|on-request|
    ///   never>` is real and verified against the installed `codex-cli
    ///   0.147.0` (`codex --help`/`codex exec --help`, both quoted in full in
    ///   the 2026-08-22 addendum to `docs/superpowers/notes/2026-07-31-
    ///   codex-cli-facts.md`) -- `never` suppresses the escalation prompt for
    ///   anything the sandbox would otherwise ask about, with the failure
    ///   reported straight back to the model instead. Not `Enforced`: in
    ///   isolation (this capability alone, at whatever sandbox mode a launch
    ///   happens to carry) it only ever changes whether a blocked action
    ///   escalates, never what the sandbox blocks in the first place --
    ///   pairing it with `--sandbox read-only` (what `AgentAdapter::
    ///   policy_args` actually does when `repo_fs_write`/`shell_exec` are
    ///   also denied) is what makes "must not attempt anything needing
    ///   approval" hold in practice; `Approval` alone, with no accompanying
    ///   sandbox restriction, would suppress prompts without necessarily
    ///   preventing anything. An `Ask` stance is `OperatorControlled` via
    ///   codex's own `approval` setting in `~/.codex/config.toml` (it appears
    ///   in `codex exec`'s stdout preamble as `approval: <value>`), which
    ///   zirv reads and never rewrites (the only bypass flag verified on the
    ///   CLI, `--dangerously-bypass-approvals-and-sandbox`, only ever
    ///   *widens*, and is never emitted by this codebase).
    /// - **Network** and **MCP/tool access** have no verified per-run flag.
    ///   `--disable <FEATURE>` is a feature-flag switch, not a tool deny-list.
    /// - **git push / destructive git** has none either, same as claude.
    fn policy_support(
        &self,
        capability: crate::commands::ctx::policy::Capability,
        stance: crate::commands::ctx::policy::Stance,
        mode: super::LaunchMode,
    ) -> crate::commands::ctx::policy::CapabilityDescriptor {
        use crate::commands::ctx::policy::{Capability, CapabilityDescriptor, Stance};

        const SANDBOX: &str = "--sandbox read-only, which scopes what an executed shell command may write rather \
             than which of codex's own tools may run (recorded facts only -- not verified against \
             a live codex CLI)";
        const WORKSPACE: &str = "--sandbox workspace-write, which keeps writes inside the workspace (documented, not \
             verified against a live codex CLI)";
        const CONFIG: &str = "codex's own `approval` setting in ~/.codex/config.toml, which zirv reads and never \
             rewrites";
        const SHELL_EXEC_DENY_UNSUPPORTED: &str = "--sandbox read-only scopes what a command may write, not whether it may run at \
             all -- a command still executes under it and can read anything the process can \
             reach (e.g. `cat ~/.aws/credentials`); codex has no verified per-run flag that \
             denies shell execution itself";
        const APPROVAL_DENY_DEGRADED: &str = "-a, --ask-for-approval never (verified on codex-cli 0.147.0), which suppresses the \
             escalation prompt for a blocked action but does not by itself decide what the \
             sandbox blocks -- paired with --sandbox read-only when repo_fs_write/shell_exec \
             are also denied, which is what actually makes this hold";
        // 2026-08-24: the interactive posture pins `--ask-for-approval
        // on-request` when the installed binary's own `--help` documents it,
        // plus `--approve-for-me` when independently advertised.
        // Degraded, never Enforced, and the wording has to carry two facts an
        // operator would otherwise assume wrongly: what actually contains the
        // damage here is the SANDBOX, not a command classifier; and codex
        // escalates on its own sandbox-boundary decision, with no per-command
        // mechanism to receive zirv's `[safety]` rules -- so read-only-SQL
        // silence and everyday-command silence are not carried onto this
        // harness the way they are onto claude.
        const APPROVAL_ASK_INTERACTIVE: &str = "-a, --ask-for-approval on-request paired with --sandbox workspace-write, probed \
             live against the installed codex-cli's own --help before it is used; when separately \
             advertised, --approve-for-me routes those boundary requests through codex's native \
             security reviewer: the sandbox is what contains damage, and codex has no per-command \
             mechanism to receive zirv's [safety] classification, so approval granularity here is \
             codex's own rather than zirv's";

        match (capability, stance) {
            (Capability::RepoFsWrite, Stance::Deny) => CapabilityDescriptor::degraded(SANDBOX),
            (Capability::OutsideRepoFsWrite, Stance::Deny) => {
                CapabilityDescriptor::degraded(WORKSPACE)
            }
            (Capability::ShellExec, Stance::Deny) => {
                CapabilityDescriptor::unsupported(SHELL_EXEC_DENY_UNSUPPORTED)
            }
            (Capability::Approval, Stance::Deny) => {
                CapabilityDescriptor::degraded(APPROVAL_DENY_DEGRADED)
            }
            (Capability::ShellExec | Capability::Approval, Stance::Ask)
                if mode.is_interactive() && self.on_request_approval_supported() =>
            {
                CapabilityDescriptor::degraded(APPROVAL_ASK_INTERACTIVE)
            }
            (Capability::ShellExec | Capability::Approval, Stance::Ask) => {
                CapabilityDescriptor::operator_controlled(CONFIG)
            }
            // Network, MCP/tool access, git operations, and every `Ask` stance
            // codex has no verified mechanism for -- see this method's own doc.
            _ => CapabilityDescriptor::advisory_only(),
        }
    }

    /// The one stance this adapter has a verified per-run mechanism for
    /// (mirroring `ClaudeAdapter::policy_args`): `RepoFsWrite`/`ShellExec` at
    /// `Deny` gets `self.read_only_args()` (`--sandbox read-only`, the same
    /// pin `distiller_cmd` already uses) **plus** `-a/--ask-for-approval
    /// never`.
    ///
    /// The second flag is new information, not in `policy_support`'s own doc
    /// comment above: `-a, --ask-for-approval <untrusted|on-request|never>`
    /// is real on both the top-level `codex [PROMPT]` launch and `codex exec`,
    /// verified against the actually-installed `codex-cli 0.147.0` at
    /// `~\AppData\Local\Programs\OpenAI\Codex\bin` (`codex --help` / `codex
    /// exec --help`, both quoted verbatim in the 2026-08-22 addendum to
    /// `docs/superpowers/notes/2026-07-31-codex-cli-facts.md`) -- the original
    /// 2026-07-31 capture of `codex exec --help` (codex-cli 0.146.0, brew) did
    /// **not** show this flag at all, so it postdates that capture; treat the
    /// addendum, not the original block, as authoritative for this flag.
    /// `never` alone would only silence the *prompt*, not what is allowed
    /// (`--dangerously-bypass-approvals-and-sandbox` is the only flag that
    /// removes the sandbox, and this never emits it); paired with `--sandbox
    /// read-only` it is what actually closes the gap `policy_support`'s own
    /// `Capability::Approval` arm now describes (`Degraded`, revised
    /// 2026-08-22 from the stale `Unsupported` this constant's name once
    /// matched): without it, a sandbox-blocked command still escalates to a
    /// human before failing, which is exactly the "codex prompts far more
    /// than claude" symptom this exists to close for a launch that has no
    /// human present to answer (a headless worker).
    ///
    /// ISSUE #134 UPDATE (2026-08-25): the second flag is no longer always
    /// the literal `--ask-for-approval never` token pair -- see
    /// `approval_suppression_args`'s own doc comment. On a headless launch
    /// (`mode` here matches `LaunchMode::Headless` exactly on every real
    /// caller: `policy_launch_args` threads through the same `mode` its own
    /// caller resolved for `headless_cmd`/`headless_cmd_stdin`) whose
    /// installed codex-cli's `codex exec --help` no longer documents the
    /// flag, this projects the config-override form instead so the launch
    /// does not fail outright with an unrecognized-argument error.
    ///
    /// **`network` (2026-08-26, codex approval-posture round) is folded into
    /// this method too, not `default_sandbox_args`**: that method's own
    /// signature (`sandbox`, `safety`, `mode` -- no `policy`) would have to
    /// change to reach `EffectivePolicy`, which is a trait-wide signature
    /// change every adapter's `impl` must match, rippling into `claude.rs`
    /// for a codex-only mapping this round is scoped to leave untouched. This
    /// method already threads `policy` through, so it is the one place that
    /// costs nothing extra to reach it from.
    ///
    /// `network` defaults to `None` (`EffectivePolicy`'s own `Option<Stance>`
    /// field, see its doc comment) when no operator layer has ever named it,
    /// which behaves exactly like `Deny`/`Ask` here -- neither adds any argv
    /// -- matching what an unwired install has always done: codex's own
    /// native default under `--sandbox workspace-write` is already
    /// `network_access: false` with no zirv-added flag at all. Only the
    /// operator's explicit `Some(Stance::Allow)` adds the
    /// `-c sandbox_workspace_write.network_access=true` config override --
    /// verified to work on both the interactive and `exec` command surfaces
    /// (`approval_suppression_args`'s own doc comment cites the same fact for
    /// `approval_policy`). Claude's own sandbox network settings are
    /// untouched by this round.
    fn policy_args(
        &self,
        policy: &crate::commands::ctx::policy::EffectivePolicy,
        mode: super::LaunchMode,
    ) -> Vec<String> {
        use crate::commands::ctx::policy::Stance;
        let mut args = if policy.repo_fs_write == Stance::Deny || policy.shell_exec == Stance::Deny
        {
            let mut args = self.read_only_args();
            args.extend(self.approval_suppression_args(mode, "never"));
            args
        } else {
            Vec::new()
        };
        if policy.network == Some(Stance::Allow) {
            args.push("-c".to_string());
            args.push("sandbox_workspace_write.network_access=true".to_string());
        }
        args
    }

    /// The codex side of the shipped-default posture, split by launch mode
    /// (2026-08-24, cross-harness permissions design).
    ///
    /// **Headless** is unchanged: `--sandbox workspace-write` paired with
    /// `--ask-for-approval never`, both verified against the installed
    /// `codex-cli 0.147.0`. Nobody is present to answer an escalation, so a
    /// blocked action is reported straight back to the model.
    ///
    /// **Interactive** upgrades the approval mode to `on-request`: the
    /// session works freely inside the workspace sandbox and escalates only
    /// when it needs to leave it. When the installed CLI also advertises
    /// `--approve-for-me`, its native security reviewer auto-clears lower-
    /// risk boundary requests, denies critical ones, and leaves only high-
    /// risk decisions to the operator. Both capabilities are probed
    /// independently so older CLIs retain the last posture they support.
    ///
    /// Deliberately **not** `untrusted`, which was this design's first
    /// answer: `untrusted` prompts for everything outside codex's own narrow
    /// built-in trusted set, which is exactly the endless-prompting failure
    /// the primary acceptance criterion exists to remove. Choosing the
    /// approval mode by how much it interrupts -- not by how much it
    /// gates -- is the whole point; the SANDBOX is what gates, and it is
    /// unchanged between the two modes.
    ///
    /// Probed, never assumed (`on_request_approval_supported`): on any doubt
    /// the launch keeps `never`, because an unrecognized argument breaks the
    /// launch outright.
    ///
    /// Never `--dangerously-bypass-approvals-and-sandbox`: that removes
    /// sandboxing entirely, which is the one thing this posture must not do.
    ///
    /// `sandbox.extra_allow`/`extra_deny` and `safety` are still ignored in
    /// both modes: they are claude permission-rule strings, and no
    /// trusted-command mechanism was verified on the installed codex CLI to
    /// receive them. Rather than invent one, this projects nothing extra and
    /// `policy_support` reports the gap as `Degraded` -- see that method's own
    /// doc comment. Faking parity here would be exactly the over-claim
    /// `policy.rs`'s honesty contract exists to prevent.
    fn default_sandbox_args(
        &self,
        sandbox: &crate::commands::ctx::config::SandboxConfig,
        safety: &crate::commands::ctx::safety::SafetyPolicy,
        mode: super::LaunchMode,
    ) -> Vec<String> {
        let _ = (sandbox, safety);
        let interactive_approval = mode.is_interactive() && self.on_request_approval_supported();
        let approval = if interactive_approval {
            "on-request"
        } else {
            "never"
        };
        let mut args = vec!["--sandbox".to_string(), "workspace-write".to_string()];
        // ISSUE #134: `--ask-for-approval` (or its `-c approval_policy=`
        // fallback on an unsupporting `codex exec`) via the shared helper --
        // see `approval_suppression_args`'s own doc comment.
        args.extend(self.approval_suppression_args(mode, approval));
        if interactive_approval && self.auto_review_supported() {
            args.push("--approve-for-me".to_string());
        }
        args
    }

    /// Issue #119 (dash worktree panes) + the mail report-back gap
    /// (2026-08-26, codex approval-posture round): see the trait method's own
    /// doc comment for what this closes and why `cwd`/`mail_dir` are not
    /// threaded through `default_sandbox_args` instead.
    ///
    /// One `-c sandbox_workspace_write.writable_roots=[...]` config override
    /// carries every extra root this launch needs, never several separate
    /// `-c` occurrences for the same key: codex's own config resolution is
    /// last-value-wins (the same fact `approval_suppression_args`'s fallback
    /// relies on for `approval_policy`), so a second `-c ...writable_roots=`
    /// would silently replace the first rather than add to it. Verified to
    /// work on both the interactive and `exec` command surfaces, the same
    /// `-c/--config key=value` fact `approval_suppression_args`'s own doc
    /// comment cites.
    ///
    /// The linked-worktree root is only added when `git_common_dir(cwd)`
    /// resolves OUTSIDE `cwd` itself -- a main checkout's own `.git` already
    /// sits inside `cwd`, and so inside `--sandbox workspace-write`'s own
    /// root, without needing to be named again. `mail_dir` is always added:
    /// it sits under the state root, always outside `cwd`, regardless of
    /// worktree shape.
    fn extra_writable_root_args(&self, cwd: &Path, mail_dir: &Path) -> Vec<String> {
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(git_dir) = super::git_common_dir(&cwd)
            && !git_dir.starts_with(&cwd)
        {
            roots.push(git_dir);
        }
        roots.push(mail_dir.to_path_buf());

        let quoted: Vec<String> = roots
            .iter()
            .map(|root| toml_quoted_string(&root.display().to_string()))
            .collect();
        vec![
            "-c".to_string(),
            format!(
                "sandbox_workspace_write.writable_roots=[{}]",
                quoted.join(",")
            ),
        ]
    }

    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let sessions_root = self.home_dir().join(".codex").join("sessions");
        let suffix = format!("-{}.jsonl", session.id);
        find_rollout(&sessions_root, &suffix)
            .unwrap_or_else(|| sessions_root.join(format!("rollout{suffix}")))
    }

    /// Derives normalized rot/usage events from the same rollout JSONL
    /// `window.rs` already parses for usage-window state (issue #86) --
    /// `window::parse_rollout_record` is the single shared parse both draw
    /// from, so the transcript is read once, not by two independent
    /// readers. Only the two shapes verified in
    /// `docs/superpowers/notes/2026-07-31-codex-cli-facts.md` ("Turn
    /// boundary", "Token usage") are mapped, onto the EXISTING
    /// `NormalizedEvent` vocabulary -- no codex-specific variant:
    ///
    /// - `task_started` -> `TurnStart` ("task_started and task_complete
    ///   bracket a turn and share a turn_id").
    /// - `task_complete` -> `AssistantFinal { text: last_agent_message
    ///   .unwrap_or_default(), input_tokens: <most recent cumulative total
    ///   this parse has seen> }`. `last_agent_message` is `None` on a
    ///   failed turn (observed as JSON `null`), which reads as empty text --
    ///   honest, since there is nothing to report, not a guess.
    /// - `token_count` (whenever `info.total_token_usage` is present) -> its
    ///   own `AssistantFinal { text: String::new(), input_tokens }`, so the
    ///   rot engine's token gate (`rot::context_tokens`, "the most recent
    ///   AssistantFinal's `input_tokens`") tracks codex's real cumulative
    ///   context size between turn boundaries too, not just at them. The
    ///   empty text never counts as a "turn" (`rot::turn_final_texts` only
    ///   counts non-empty text) and never touches the marker signal, which
    ///   stays capability-gated off for codex regardless
    ///   (`capabilities().marker_signal == false`) -- so this mapping has no
    ///   effect on `marker_miss_rate`/the weighted score's marker term.
    ///
    /// NOT mapped, deliberately, because no verified rollout shape exists
    /// for them: tool calls, tool results, and any compaction/summarization
    /// boundary. `ToolCall`/`ToolResult`/`Compaction` are simply never
    /// emitted, so `tool_failure_rate`/`repetition_hits` always read `0.0`
    /// for a codex session and the token gate's "at or above token_ceiling"
    /// rule is the only escalation path that still fires -- see Known
    /// Issues.
    ///
    /// The one bit of cross-line state (`last_tokens`, local to this single
    /// call) is a deliberate, bounded residual against the trait's
    /// "line-local" ideal: if an incremental poll's chunk boundary happens
    /// to split between a `token_count` line and the `task_complete` line
    /// for the same turn, that turn's `AssistantFinal` reports whatever
    /// `last_tokens` this call has seen so far (`0` if nothing yet) rather
    /// than the true cumulative count -- self-correcting at the very next
    /// `token_count` line, which arrives frequently in practice. `rot.rs`
    /// itself never sees or knows about this: it only ever receives the
    /// resulting `NormalizedEvent`s.
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        let mut events = Vec::new();
        let mut last_tokens: u64 = 0;
        for line in jsonl.lines() {
            match window::parse_rollout_record(line) {
                Some(RolloutRecord::TaskStarted) => {
                    events.push(NormalizedEvent::TurnStart);
                }
                Some(RolloutRecord::TaskComplete { last_agent_message }) => {
                    events.push(NormalizedEvent::AssistantFinal {
                        text: last_agent_message.unwrap_or_default(),
                        input_tokens: last_tokens,
                    });
                }
                Some(RolloutRecord::TokenCount {
                    totals: Some(totals),
                    ..
                }) => {
                    last_tokens = totals.input_tokens;
                    events.push(NormalizedEvent::AssistantFinal {
                        text: String::new(),
                        input_tokens: last_tokens,
                    });
                }
                _ => {}
            }
        }
        events
    }

    /// Only `assistant_texts` is populated, from verified
    /// `task_complete.last_agent_message` lines -- the one piece of real
    /// transcript content the rollout format gives a verified shape for
    /// (see `parse_events`'s own doc comment).
    /// `user_messages`/`files_touched`/`tool_errors` stay empty: no
    /// verified rollout shape carries them, and inventing one would be
    /// exactly the fabricated-content class `handoff.rs`'s own eventless
    /// guard exists to prevent -- this is a real, if partial, structural
    /// context now, not the permanent empty stub it used to be.
    /// `last_n` truncation mirrors `claude::structural_context`'s own
    /// `keep_last`: `last_n == 0` keeps nothing at all.
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        let mut assistant_texts: Vec<String> = jsonl
            .lines()
            .filter_map(|line| match window::parse_rollout_record(line) {
                Some(RolloutRecord::TaskComplete {
                    last_agent_message: Some(text),
                }) if !text.trim().is_empty() => Some(text),
                _ => None,
            })
            .collect();
        if assistant_texts.len() > last_n {
            assistant_texts.drain(..assistant_texts.len() - last_n);
        }
        StructuralContext {
            assistant_texts,
            ..StructuralContext::default()
        }
    }

    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        let mut latest = None;
        for line in jsonl.lines() {
            if let Some(RolloutRecord::TokenCount {
                totals: Some(totals),
                ..
            }) = window::parse_rollout_record(line)
            {
                latest = Some(TranscriptUsage {
                    input_tokens: totals.input_tokens,
                    // `RolloutTokenTotals` has no cache-class fields at all --
                    // a guessed class would be worse than an honest zero.
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    output_tokens: totals.output_tokens,
                });
            }
        }
        latest
    }

    fn transcript_usage_is_cumulative(&self) -> bool {
        true
    }

    fn compact_command(&self) -> Option<&'static str> {
        None
    }

    fn quit_sequence(&self) -> &'static str {
        "/quit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Codex still gets no marker signal (spec-mandated, unrelated to
            // event parsing) and `token_usage` describes a different,
            // separate mechanism -- see `window::refresh_codex_usage`'s own
            // doc comment on that nuance. `turn_signal` stays false too:
            // `register_turn_signal` is still a no-op, unrelated to whether
            // a transcript can be parsed after the fact.
            marker_signal: false,
            token_usage: false,
            turn_signal: false,
            // The adapter supports this generally. `system_prompt_supported`
            // narrows the answer for Windows shell-shim launch shapes.
            system_prompt: true,
            // Issue #86 (2026-08-23): `parse_events`/`structural_context`
            // now derive real turn-boundary and token data from the rollout
            // JSON (see their own doc comments for exactly what is and is
            // not mapped), so this is honestly `true` -- rot scoring,
            // `zirv ctx status`'s usage/rot cells, and the pacing gate all
            // light up for a codex session.
            events: true,
            // Issue #118: verified against codex's own ratatui composer
            // (issue #114) -- a same-burst text+`\r` is read as a paste and
            // the `\r` is folded into the pasted text rather than submitted.
            defer_injection_submit: true,
            // Issue #155: no capacity is verified for codex, and a guessed
            // one is worse than falling back to rot's absolute defaults,
            // which are at least a known quantity. Never fake parity.
            context_window_tokens: None,
        }
    }

    fn system_prompt_supported(&self, launch: &[String]) -> bool {
        let probe = if launch.is_empty() {
            super::flatten_command(self.interactive_cmd(None, &[]))
        } else {
            launch.to_vec()
        };
        !super::launch_reparses_through_shim(&probe)
    }

    /// Verified (docs/superpowers/notes/2026-07-31-codex-cli-facts.md, line
    /// 139): `-m, --model <MODEL>` is present on top-level `codex --help`
    /// with the same description as on `codex exec --help`, so the
    /// interactive launch this feeds (`interactive_cmd`) accepts it too.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }

    // No `resume_args` override: codex's own resume story (a `--last`/
    // session-id flag for the interactive launch) is unverified against the
    // real CLI, unlike `model_args` above. The trait default (`None`) is
    // correct here -- `dash::roster::restore_argv` falls back to a plain
    // prompt-carrying relaunch for this adapter rather than a guessed flag.
    //
    // No `session_pin_args` override either, for the same reason and one
    // stronger: `headless_cmd` above already records the verified fact that
    // codex has no `--session-id` flag at all and always mints its own id, so
    // there is nothing to pin an interactive dashboard pane with. The trait
    // default (empty) is what "no verified mechanism" has to ship as.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::{AgentAdapter, select};
    use crate::commands::ctx::config::CtxConfig;

    /// I: `super::super::built_args` (`adapters/mod.rs`) takes the program
    /// string rather than the whole adapter, since `program` is private to
    /// this module -- this thin wrapper is what lets every call site below
    /// keep passing `&adapter` unchanged. Mirrors `claude.rs`'s own wrapper.
    fn built_args(adapter: &CodexAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(&adapter.program, cmd)
    }

    #[test]
    fn codex_detects_its_own_binary() {
        let adapter = CodexAdapter::new(None);
        assert!(adapter.detect(&["/usr/local/bin/codex".to_string()]));
        assert!(!adapter.detect(&["/usr/local/bin/claude".to_string()]));
    }

    #[test]
    fn codex_has_no_marker_signal() {
        let caps = CodexAdapter::new(None).capabilities();
        assert!(!caps.marker_signal, "the spec gives codex no marker signal");
    }

    /// Codex reports NO capacity: none is verified for it, and a guessed
    /// capacity is worse than falling back to the absolute defaults, which
    /// are at least a known quantity. Never fake parity.
    #[test]
    fn codex_reports_no_context_window_because_none_is_verified() {
        assert_eq!(CodexAdapter::new(None).context_window_tokens(None), None);
        assert_eq!(
            CodexAdapter::new(None).capabilities().context_window_tokens,
            None
        );
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join(name),
        )
        .expect("fixture must be committed")
    }

    /// Issue #86 acceptance: capabilities().events is honestly true now that
    /// parse_events derives real data from the rollout JSON.
    #[test]
    fn codex_now_reports_verified_event_parsing() {
        assert!(CodexAdapter::new(None).capabilities().events);
    }

    /// Issue #86: the exact normalized event sequence a recorded two-turn
    /// codex rollout fixture must produce -- turn boundaries from
    /// task_started/task_complete, tokens from every token_count snapshot,
    /// and a failed turn's null last_agent_message reading as empty text
    /// rather than being skipped or guessed at.
    #[test]
    fn parse_events_derives_turn_boundaries_and_token_updates_from_the_rollout_json() {
        let jsonl = fixture("codex-rollout-turn-events.jsonl");
        let adapter = CodexAdapter::new(None);
        let events = adapter.parse_events(&jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::TurnStart,
                NormalizedEvent::AssistantFinal {
                    text: String::new(),
                    input_tokens: 1200,
                },
                NormalizedEvent::AssistantFinal {
                    text: "[zirv] wired the webhook route".to_string(),
                    input_tokens: 1200,
                },
                NormalizedEvent::TurnStart,
                NormalizedEvent::AssistantFinal {
                    text: String::new(),
                    input_tokens: 3400,
                },
                NormalizedEvent::AssistantFinal {
                    text: String::new(),
                    input_tokens: 3400,
                },
            ],
            "got {events:?}"
        );
    }

    /// Issue #86: the derived stream must actually light up the rot engine
    /// end to end -- a real context-token reading and a real (here,
    /// healthy: everything is well under the default token_floor) verdict,
    /// not the "no data" refusal an eventless adapter gets.
    #[test]
    fn a_derived_event_stream_scores_through_the_rot_engine() {
        let jsonl = fixture("codex-rollout-turn-events.jsonl");
        let adapter = CodexAdapter::new(None);
        let events = adapter.parse_events(&jsonl);
        assert_eq!(crate::commands::ctx::rot::context_tokens(&events), 3400);

        let score = crate::commands::ctx::rot::score_events(
            &events,
            adapter.capabilities(),
            &crate::commands::ctx::config::ScoreConfig::default(),
        );
        assert_eq!(score.context_tokens, 3400);
        assert_eq!(
            score.verdict,
            crate::commands::ctx::rot::Verdict::Healthy,
            "well under the default token_floor: {score:?}"
        );
    }

    /// Issue #86: `structural_context` is no longer the permanent empty
    /// stub -- a successful turn's `last_agent_message` shows up as real
    /// assistant text, while a failed turn's `null` message contributes
    /// nothing (never a fabricated empty-string entry).
    #[test]
    fn structural_context_carries_the_verified_assistant_text_only() {
        let jsonl = fixture("codex-rollout-turn-events.jsonl");
        let adapter = CodexAdapter::new(None);
        let ctx = adapter.structural_context(&jsonl, 10);
        assert_eq!(
            ctx.assistant_texts,
            vec!["[zirv] wired the webhook route".to_string()]
        );
        assert!(
            ctx.user_messages.is_empty()
                && ctx.files_touched.is_empty()
                && ctx.tool_errors.is_empty(),
            "no verified rollout shape backs these fields yet: {ctx:?}"
        );
    }

    #[test]
    fn codex_injects_composed_context_with_the_official_config_override() {
        let adapter = CodexAdapter::new(None);
        let args = adapter.system_prompt_args("be consistent\nacross turns");
        assert_eq!(args[0], "-c");
        assert_eq!(
            args[1],
            "developer_instructions=\"be consistent\\nacross turns\""
        );
        assert!(adapter.capabilities().system_prompt);
        assert!(adapter.system_prompt_supported(&[]));
        assert_eq!(
            adapter.user_system_prompt_flag(),
            None,
            "generic -c overrides are not a dedicated user prompt flag"
        );
    }

    /// Codex is now supported out of the box: `--agent codex` selects it
    /// directly wherever the `codex` program resolves, the same contract
    /// `ClaudeAdapter::ready` already gives claude.
    #[test]
    fn selecting_codex_succeeds_once_the_binary_resolves() {
        let adapter =
            select(Some("codex"), &[], &CtxConfig::default()).expect("codex resolves and is ready");
        assert_eq!(adapter.name(), "codex");
    }

    /// Mirrors `ClaudeAdapter::ready`'s own contract exactly:
    /// `resolve_program` is the only thing that can fail it, and a bare
    /// `"codex"` that never resolves to anything at all is not an error here
    /// (the OS raises its own "not found" at spawn time instead).
    #[test]
    fn ready_succeeds_for_the_default_program_name() {
        assert!(CodexAdapter::new(None).ready().is_ok());
    }

    /// argv auto-detection now selects codex directly instead of refusing:
    /// once `ready()` no longer hard-errors, `select`'s detection arm has
    /// nothing left to refuse on for a plain `codex ...` command.
    #[test]
    fn detecting_codex_argv_selects_codex() {
        let cmd = vec!["codex".to_string(), "exec".to_string(), "do it".to_string()];
        let adapter = select(None, &cmd, &CtxConfig::default()).expect("codex is ready");
        assert_eq!(adapter.name(), "codex");
    }

    /// Verified via `codex exec --help`: there is no `--session-id` flag, so
    /// the session parameter cannot appear in the built command at all.
    #[test]
    fn headless_cmd_uses_exec_and_has_no_session_flag() {
        let adapter = CodexAdapter::new(Some("/tmp/fake-codex"));
        let cmd = adapter.headless_cmd(
            "do the work",
            &SessionId::parse("abc"),
            &["--model".to_string(), "gpt-5.6-luna".to_string()],
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "/tmp/fake-codex");
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "gpt-5.6-luna".to_string(),
            ],
            "codex exec takes no session flag; codex mints its own session id"
        );
    }

    /// Verified via `codex --help`: with no subcommand, the prompt goes
    /// straight to the interactive CLI, exactly like claude's positional form.
    #[test]
    fn interactive_cmd_passes_the_initial_prompt_positionally_with_no_subcommand() {
        let adapter = CodexAdapter::new(None);
        let with = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(built_args(&adapter, &with), vec!["resume this".to_string()]);

        let without = adapter.interactive_cmd(None, &["--last".to_string()]);
        assert_eq!(built_args(&adapter, &without), vec!["--last".to_string()]);
    }

    /// Verified via `codex exec --help` (quoted verbatim in the notes file):
    /// `-m, --model <MODEL>` is a real flag, and the prompt is read from
    /// stdin when omitted, so the distiller never needs an argv prompt.
    #[test]
    fn distiller_cmd_uses_exec_with_a_cheap_model_and_reads_stdin() {
        let adapter = CodexAdapter::new(None).with_ignore_flags_forced(false);
        let cmd = adapter.distiller_cmd("gpt-5.6-luna");
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "exec".to_string(),
                "--model".to_string(),
                "gpt-5.6-luna".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string(),
            ]
        );
    }

    /// C: an empty `model` (no operator config, and `default_distiller_
    /// model` is `None` for codex) must omit `--model` entirely rather than
    /// pass an empty value -- codex's own `~/.codex/config.toml` default
    /// then applies instead of zirv guessing a model name.
    #[test]
    fn distiller_cmd_omits_the_model_flag_when_none_is_given() {
        let adapter = CodexAdapter::new(None).with_ignore_flags_forced(false);
        let cmd = adapter.distiller_cmd("");
        let args = built_args(&adapter, &cmd);
        assert!(
            !args.iter().any(|a| a == "--model"),
            "no model resolved means no --model flag at all: {args:?}"
        );
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string()
            ]
        );
    }

    /// C: codex has no verified cheap-model default of its own -- a
    /// hardcoded model name is specific to claude's lineup.
    #[test]
    fn codex_has_no_default_distiller_model() {
        assert_eq!(CodexAdapter::new(None).default_distiller_model(), None);
    }

    /// Codex has no adapter-owned hard default for a delegated worker
    /// either: its own CLI/config default applies untouched when the
    /// operator has not set `worker.codex` -- see
    /// `adapters::resolve_worker_model`.
    #[test]
    fn codex_has_no_default_worker_model() {
        assert_eq!(CodexAdapter::new(None).default_worker_model(), None);
    }

    /// Same "nothing verified to guess" answer for the role layers: codex
    /// contributes neither an orchestrator nor a worker layer of its own, so
    /// `prompt::with_adapter_layer` splices nothing in for either role rather
    /// than handing codex text written for claude's tools.
    #[test]
    fn codex_contributes_no_worker_layer_of_its_own() {
        assert_eq!(CodexAdapter::new(None).worker_system_prompt(), None);
    }

    /// The codex ladder, top to bottom: `gpt-5.6-sol` (the default when no
    /// `-m` is given), `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.4-mini` --
    /// verified via `codex debug models` in docs/superpowers/notes/
    /// 2026-07-31-codex-cli-facts.md's "Cheap model alias for distillation"
    /// section, sourced from a 0.146.0 capture; codex is not installed on
    /// this machine to re-verify the catalog against 0.105.0 (the version
    /// most operators actually get, per `distiller_cmd`'s own doc comment),
    /// so treat this ladder as unverified for that version specifically --
    /// the cited note documents the `--ignore-rules`/`--ignore-user-config`
    /// gap only, not this catalog. `review_model_below` returns the tier one
    /// below `seat`; an unknown or absent seat assumes the top tier
    /// (`gpt-5.6-sol`), and `gpt-5.4-mini` (already the floor) maps to
    /// itself.
    #[test]
    fn review_model_below_walks_the_codex_ladder() {
        let adapter = CodexAdapter::new(None);
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.6-sol")),
            "gpt-5.6-terra"
        );
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.6-terra")),
            "gpt-5.6-luna"
        );
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.6-luna")),
            "gpt-5.4-mini"
        );
        assert_eq!(
            adapter.review_model_below(Some("gpt-5.4-mini")),
            "gpt-5.4-mini"
        );
        assert_eq!(
            adapter.review_model_below(None),
            "gpt-5.6-terra",
            "no seat configured: assume the top tier"
        );
        assert_eq!(
            adapter.review_model_below(Some("claude-fable-5")),
            "gpt-5.6-terra",
            "a seat naming another adapter's model is unrecognised: assume the top tier"
        );
    }

    /// Seat matching must be case-insensitive: a mixed-case seat must land
    /// on the same ladder rung as its lowercase form, not fall through to
    /// the unknown arm and assume the top tier.
    #[test]
    fn review_model_below_matches_the_seat_case_insensitively() {
        let adapter = CodexAdapter::new(None);
        assert_eq!(
            adapter.review_model_below(Some("GPT-5.6-Sol")),
            "gpt-5.6-terra"
        );
        assert_eq!(
            adapter.review_model_below(Some("Gpt-5.6-Terra")),
            "gpt-5.6-luna"
        );
        assert_eq!(
            adapter.review_model_below(Some("GPT-5.6-LUNA")),
            "gpt-5.4-mini"
        );
        assert_eq!(
            adapter.review_model_below(Some("GPT-5.4-Mini")),
            "gpt-5.4-mini"
        );
    }

    /// The rollout `TokenCount` fixture both cumulative-snapshot tests below
    /// share: two snapshots, the second superseding the first.
    const CODEX_TOKEN_COUNT_FIXTURE: &str = concat!(
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":2}}}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":25,"output_tokens":6}}}}"#,
    );

    #[test]
    fn transcript_usage_uses_the_latest_cumulative_token_snapshot() {
        let adapter = CodexAdapter::new(None);
        assert_eq!(
            adapter.transcript_usage(CODEX_TOKEN_COUNT_FIXTURE),
            Some(TranscriptUsage {
                input_tokens: 25,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 6,
            })
        );
        assert!(adapter.transcript_usage_is_cumulative());
    }

    /// Codex's rollout `TokenCount` totals expose no cache classes at all, so
    /// its two new fields stay 0 and `context_total()` degrades to exactly
    /// today's number. Never guess a class an adapter does not report.
    #[test]
    fn codex_reports_zero_for_the_cache_classes_it_cannot_see() {
        let adapter = CodexAdapter::new(None);
        let usage = adapter
            .transcript_usage(CODEX_TOKEN_COUNT_FIXTURE)
            .expect("usage");
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.context_total(), usage.input_tokens);
    }

    /// B: `--sandbox read-only` (verified against `codex exec --help` on
    /// codex-cli 0.105.0: `-s, --sandbox <SANDBOX_MODE>`, possible values
    /// `read-only`/`workspace-write`/`danger-full-access`) is the pin behind
    /// `zirv ctx optimize`'s report-only guarantee for a codex judgment
    /// child, the same role claude's `--disallowedTools=...` plays. Pinned
    /// as its own test so a future edit to `distiller_cmd` cannot drop the
    /// flag without a test failing here specifically.
    #[test]
    fn the_distiller_is_pinned_to_the_read_only_sandbox() {
        let adapter = CodexAdapter::new(None).with_ignore_flags_forced(false);
        let cmd = adapter.distiller_cmd("gpt-5.6-luna");
        let args = built_args(&adapter, &cmd);
        assert!(
            args.windows(2).any(|w| w == ["--sandbox", "read-only"]),
            "the distiller must be pinned to codex's own read-only sandbox: {args:?}"
        );
    }

    /// Issue #89: with a codex version known to support them, `--ignore-
    /// rules --ignore-user-config` must appear in the constructed argv,
    /// both directly (`read_only_args`) and through `distiller_cmd`, which
    /// builds on it.
    #[test]
    fn read_only_args_adds_the_ignore_flags_when_the_installed_codex_supports_them() {
        let adapter = CodexAdapter::new(None).with_ignore_flags_forced(true);
        assert_eq!(
            adapter.read_only_args(),
            vec![
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--ignore-rules".to_string(),
                "--ignore-user-config".to_string(),
            ]
        );

        let cmd = adapter.distiller_cmd("gpt-5.6-luna");
        let args = built_args(&adapter, &cmd);
        assert!(
            args.windows(2)
                .any(|w| w == ["--ignore-rules", "--ignore-user-config"]),
            "got {args:?}"
        );
    }

    /// Issue #89: with an unknown or older codex version, the ignore flags
    /// must not appear at all -- passing either on an install that does not
    /// recognize it is very likely an unrecognized-argument error.
    #[test]
    fn read_only_args_omits_the_ignore_flags_when_unsupported() {
        let adapter = CodexAdapter::new(None).with_ignore_flags_forced(false);
        assert_eq!(
            adapter.read_only_args(),
            vec!["--sandbox".to_string(), "read-only".to_string()]
        );
    }

    /// Issue #89: `sandbox_residual_note` is the flip side of the flags
    /// above -- present (naming the residual) exactly when the flags are
    /// absent, `None` exactly when they are present.
    #[test]
    fn sandbox_residual_note_tracks_ignore_flag_support() {
        let unsupported = CodexAdapter::new(None).with_ignore_flags_forced(false);
        let note = unsupported
            .sandbox_residual_note()
            .expect("a residual to report");
        assert!(note.contains("--ignore-rules"), "got {note}");
        assert!(note.contains("--ignore-user-config"), "got {note}");

        let supported = CodexAdapter::new(None).with_ignore_flags_forced(true);
        assert_eq!(
            supported.sandbox_residual_note(),
            None,
            "nothing to disclose once the installed codex-cli supports both flags"
        );
    }

    /// Bug B: the shipped default `[policy]` (all `Allow`) must leave a real
    /// launch byte-for-byte unaffected, exactly like claude's own
    /// `policy_args_is_empty_under_the_default_all_allow_policy`.
    #[test]
    fn policy_args_is_empty_under_the_default_all_allow_policy() {
        let adapter = CodexAdapter::new(None);
        assert!(
            adapter
                .policy_args(
                    &crate::commands::ctx::policy::EffectivePolicy::default(),
                    super::super::LaunchMode::Headless
                )
                .is_empty()
        );
    }

    /// `network` (2026-08-26, codex approval-posture round): `None`
    /// (including `EffectivePolicy::default()`'s own unconfigured value),
    /// `Deny` and `Ask` all add nothing here -- codex's own native default
    /// under `--sandbox workspace-write` is already closed, matching what an
    /// unwired install has always done. Only the operator's explicit
    /// `Some(Stance::Allow)` adds the config override, and as one `-c`
    /// occurrence -- never a bare `network_access` substring split across two
    /// tokens some other way.
    #[test]
    fn policy_args_adds_the_network_override_only_when_policy_explicitly_allows_it() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = CodexAdapter::new(None);

        let denied = adapter.policy_args(
            &EffectivePolicy::default(),
            super::super::LaunchMode::Headless,
        );
        assert!(
            !denied.iter().any(|a| a.contains("network_access")),
            "network must stay closed under the default policy: {denied:?}"
        );

        let allowed_policy = EffectivePolicy {
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        let allowed = adapter.policy_args(&allowed_policy, super::super::LaunchMode::Headless);
        assert!(
            allowed
                .windows(2)
                .any(|w| w == ["-c", "sandbox_workspace_write.network_access=true"]),
            "got {allowed:?}"
        );
    }

    /// `-a, --ask-for-approval <untrusted|on-request|never>`, verified
    /// against the installed `codex-cli 0.147.0`'s own `--help`/`exec --help`
    /// (see `policy_args`'s own doc comment and the 2026-08-22 addendum to
    /// `docs/superpowers/notes/2026-07-31-codex-cli-facts.md`): pairing
    /// `--sandbox read-only` with `--ask-for-approval never` is what actually
    /// stops a Deny-policy launch from escalating to a human before the
    /// sandbox denies the command anyway -- `--sandbox` alone only decides
    /// what is *allowed*, not whether a blocked attempt still prompts first.
    #[test]
    fn policy_args_pins_the_read_only_sandbox_and_suppresses_approval_prompts_when_shell_exec_is_denied()
     {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = CodexAdapter::new(None)
            .with_ignore_flags_forced(false)
            .with_exec_ask_for_approval_forced(true);
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        assert_eq!(
            adapter.policy_args(&policy, super::super::LaunchMode::Headless),
            vec![
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ]
        );
    }

    #[test]
    fn policy_args_pins_the_read_only_sandbox_and_suppresses_approval_prompts_when_repo_fs_write_is_denied()
     {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = CodexAdapter::new(None)
            .with_ignore_flags_forced(false)
            .with_exec_ask_for_approval_forced(true);
        let policy = EffectivePolicy {
            repo_fs_write: Stance::Deny,
            ..EffectivePolicy::default()
        };
        assert_eq!(
            adapter.policy_args(&policy, super::super::LaunchMode::Headless),
            vec![
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ]
        );
    }

    /// Issue #134: when the installed codex-cli's own `codex exec --help`
    /// no longer documents `--ask-for-approval` (current codex-cli 0.149.x),
    /// a headless Deny-policy launch must fall back to the config-override
    /// form rather than pass the rejected flag -- the whole point of the
    /// fix, driven entirely through the forcing seam so this never depends
    /// on whatever codex happens to be installed on the machine running the
    /// test suite.
    #[test]
    fn policy_args_falls_back_to_the_config_override_when_exec_rejects_the_approval_flag() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = CodexAdapter::new(None)
            .with_ignore_flags_forced(false)
            .with_exec_ask_for_approval_forced(false);
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let args = adapter.policy_args(&policy, super::super::LaunchMode::Headless);
        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "read-only".to_string(),
                "-c".to_string(),
                "approval_policy=never".to_string(),
            ]
        );
        assert!(
            !args.iter().any(|a| a == "--ask-for-approval"),
            "must not pass the flag the installed codex-cli rejects: {args:?}"
        );
    }

    /// The interactive path is unaffected by the `exec`-scoped probe: even
    /// when `codex exec --help` would not document the flag, the top-level
    /// interactive launch still accepts it, so `policy_args` must keep
    /// emitting the plain flag pair there.
    #[test]
    fn policy_args_keeps_the_plain_flag_on_an_interactive_launch_regardless_of_the_exec_probe() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = CodexAdapter::new(None)
            .with_ignore_flags_forced(false)
            .with_exec_ask_for_approval_forced(false);
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        assert_eq!(
            adapter.policy_args(&policy, super::super::LaunchMode::Interactive),
            vec![
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ]
        );
    }

    /// `Ask` stays `OperatorControlled` (`policy_support`'s own `CONFIG`
    /// arm): codex has no verified per-run mechanism for it either, so
    /// `policy_args` must not invent one -- an operator who wants
    /// per-command prompting still configures it in `~/.codex/config.toml`.
    #[test]
    fn policy_args_leaves_ask_untouched() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = CodexAdapter::new(None);
        let policy = EffectivePolicy {
            shell_exec: Stance::Ask,
            ..EffectivePolicy::default()
        };
        assert!(
            adapter
                .policy_args(&policy, super::super::LaunchMode::Headless)
                .is_empty()
        );
    }

    /// This never emits `--dangerously-bypass-approvals-and-sandbox` (the
    /// only flag that actually removes sandboxing) under any policy input --
    /// a Deny policy must only ever get *more* restrictive, never that.
    #[test]
    fn policy_args_never_emits_the_dangerous_bypass_flag() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = CodexAdapter::new(None);
        for policy in [
            EffectivePolicy::default(),
            EffectivePolicy {
                shell_exec: Stance::Deny,
                ..EffectivePolicy::default()
            },
            EffectivePolicy {
                repo_fs_write: Stance::Deny,
                shell_exec: Stance::Deny,
                approval: Stance::Deny,
                ..EffectivePolicy::default()
            },
        ] {
            let args = adapter.policy_args(&policy, super::super::LaunchMode::Headless);
            assert!(
                !args
                    .iter()
                    .any(|a| a.contains("dangerously-bypass-approvals-and-sandbox")),
                "must never widen: {args:?}"
            );
        }
    }

    /// The spec's revised codex-interactive projection (2026-08-24).
    /// `on-request`, NOT `untrusted`: `untrusted` prompts for everything
    /// outside codex's own narrow trusted set, which is precisely the
    /// endless-prompting failure the primary acceptance criterion forbids.
    /// Driven through the forcing seam, never a live probe: asserting an
    /// exact argv that depends on whatever `codex` happens to be installed
    /// on the test machine is the probe-dependent assertion this repo
    /// forbids.
    #[test]
    fn an_interactive_launch_uses_the_low_noise_on_request_approval_mode() {
        let adapter = CodexAdapter::new(None)
            .with_on_request_approval_forced(true)
            .with_auto_review_forced(true);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Interactive,
        );
        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "on-request".to_string(),
                "--approve-for-me".to_string(),
            ]
        );
        assert!(
            !args.iter().any(|a| a == "untrusted"),
            "untrusted is the noisy polarity this task exists to avoid: {args:?}"
        );
    }

    #[test]
    fn interactive_auto_review_is_capability_probed_and_never_used_headlessly() {
        for (mode, supported, expected) in [
            (super::super::LaunchMode::Interactive, true, true),
            (super::super::LaunchMode::Interactive, false, false),
            (super::super::LaunchMode::Headless, true, false),
        ] {
            let adapter = CodexAdapter::new(None)
                .with_on_request_approval_forced(true)
                .with_auto_review_forced(supported);
            let args = adapter.default_sandbox_args(&Default::default(), &Default::default(), mode);
            assert_eq!(
                args.iter().any(|arg| arg == "--approve-for-me"),
                expected,
                "mode={mode:?}, supported={supported}: {args:?}"
            );
        }
    }

    /// The fail-closed half: an installed codex whose own `--help` does not
    /// document `on-request` gets the posture it always had. zirv must never
    /// pass a value the binary may reject -- an unrecognized argument breaks
    /// the launch outright, which is worse than the prompt behaviour it was
    /// meant to tune.
    #[test]
    fn an_interactive_launch_falls_back_to_never_when_the_probe_is_unsure() {
        let adapter = CodexAdapter::new(None)
            .with_on_request_approval_forced(false)
            .with_auto_review_forced(false);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Interactive,
        );
        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ]
        );
    }

    /// Headless is unchanged in both directions: nobody is present, so
    /// `on-request` would stall the run waiting on an answer that never
    /// comes, even on an install that supports it.
    #[test]
    fn a_headless_launch_keeps_never_regardless_of_what_the_probe_says() {
        for supported in [true, false] {
            let adapter = CodexAdapter::new(None)
                .with_on_request_approval_forced(supported)
                .with_auto_review_forced(supported)
                .with_exec_ask_for_approval_forced(true);
            let args = adapter.default_sandbox_args(
                &Default::default(),
                &Default::default(),
                super::super::LaunchMode::Headless,
            );
            assert_eq!(
                args,
                vec![
                    "--sandbox".to_string(),
                    "workspace-write".to_string(),
                    "--ask-for-approval".to_string(),
                    "never".to_string(),
                ],
                "probe={supported}"
            );
        }
    }

    /// The invariant that holds whatever is installed -- the assertion that
    /// is safe to make without the forcing seam.
    ///
    /// ISSUE #134 UPDATE (2026-08-25): index 2 is no longer always
    /// `--ask-for-approval` -- a headless launch against a live installed
    /// `codex` whose `exec --help` no longer documents the flag falls back
    /// to `-c` (`approval_suppression_args`'s own doc comment). Both are
    /// valid approval-suppression mechanisms; this only asserts one of them
    /// is present and the sandbox is never removed, exactly the assertion
    /// shape this test's own name promises: safe without the forcing seam.
    #[test]
    fn every_codex_posture_keeps_the_explicit_workspace_sandbox_pair() {
        let adapter = CodexAdapter::new(None);
        for mode in [
            super::super::LaunchMode::Interactive,
            super::super::LaunchMode::Headless,
        ] {
            let args = adapter.default_sandbox_args(&Default::default(), &Default::default(), mode);
            assert!(args.len() >= 4, "got {args:?}");
            assert_eq!(
                &args[0..2],
                &["--sandbox".to_string(), "workspace-write".to_string()]
            );
            assert!(
                args[2] == "--ask-for-approval" || args[2] == "-c",
                "expected an approval-suppression flag at index 2: {args:?}"
            );
            assert!(
                !args
                    .iter()
                    .any(|a| a.contains("dangerously-bypass-approvals-and-sandbox")),
                "must never remove the sandbox: {args:?}"
            );
        }
    }

    /// zirv projects NOTHING of `[safety]` onto codex: no trusted-command
    /// configuration was verified to exist on the installed CLI, and the spec
    /// is explicit that on any doubt the adapter projects nothing extra and
    /// reports the gap rather than faking parity with claude.
    #[test]
    fn codex_projects_no_per_command_safety_rules_at_all() {
        let adapter = CodexAdapter::new(None)
            .with_on_request_approval_forced(true)
            .with_auto_review_forced(false);
        let safety = crate::commands::ctx::safety::SafetyPolicy::default();
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &safety,
            super::super::LaunchMode::Interactive,
        );
        assert!(
            !args
                .iter()
                .any(|a| a.contains("rm -rf") || a.contains("git push")),
            "no command-level rule may leak into codex argv: {args:?}"
        );
    }

    /// The shipped-default posture (2026-08-22): workspace-write (commands
    /// run freely inside the repo) paired with never-ask (no escalation
    /// prompt), verified against the real installed `codex-cli 0.147.0`.
    #[test]
    fn default_sandbox_args_pairs_workspace_write_with_never_ask() {
        let adapter = CodexAdapter::new(None).with_exec_ask_for_approval_forced(true);
        assert_eq!(
            adapter.default_sandbox_args(
                &Default::default(),
                &Default::default(),
                super::super::LaunchMode::Headless,
            ),
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
            ]
        );
    }

    /// Issue #134: on a headless launch whose installed codex-cli's own
    /// `codex exec --help` no longer documents `--ask-for-approval` (current
    /// codex-cli 0.149.x), the shipped-default posture must still suppress
    /// approval prompts -- via the `-c approval_policy=never` fallback --
    /// rather than pass the flag the binary rejects outright and break the
    /// launch.
    #[test]
    fn default_sandbox_args_falls_back_to_the_config_override_when_exec_rejects_the_approval_flag()
    {
        let adapter = CodexAdapter::new(None).with_exec_ask_for_approval_forced(false);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "-c".to_string(),
                "approval_policy=never".to_string(),
            ]
        );
    }

    /// The interactive path is unaffected by the `exec`-scoped probe, even
    /// when it says unsupported: `--sandbox workspace-write` alongside
    /// `on-request` still uses the plain flag, because the top-level
    /// interactive `codex` launch is a different command surface that still
    /// accepts it.
    #[test]
    fn default_sandbox_args_interactive_ignores_the_exec_scoped_probe() {
        let adapter = CodexAdapter::new(None)
            .with_on_request_approval_forced(true)
            .with_auto_review_forced(false)
            .with_exec_ask_for_approval_forced(false);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Interactive,
        );
        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "on-request".to_string(),
            ]
        );
    }

    /// Must never be the flag that removes sandboxing entirely.
    #[test]
    fn default_sandbox_args_never_emits_the_dangerous_bypass_flag() {
        let adapter = CodexAdapter::new(None);
        let args = adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            super::super::LaunchMode::Headless,
        );
        assert!(
            !args
                .iter()
                .any(|a| a.contains("dangerously-bypass-approvals-and-sandbox")),
            "must never widen: {args:?}"
        );
    }

    /// Issue #119 + the mail report-back gap (2026-08-26): a launch inside a
    /// PLAIN checkout (not a linked worktree) must still get the mail
    /// subtree as a writable root -- `zirv ctx send` report-back always
    /// needs it -- but must never name the state root itself: only the
    /// invariant is asserted (some writable-root arg naming the mail dir),
    /// never exact argv, since the config-override string this builds is an
    /// internal choice a test must not lock in more tightly than the
    /// mechanism itself is verified.
    ///
    /// A real bug found running this branch's own gates on Windows
    /// (2026-08-26): `toml_quoted_string`'s basic-string form embeds a
    /// literal `"` in every writable-root argv token it builds, and
    /// `guard_cmd_shim_reparse`'s `CMD_REPARSE_METACHARS` refuses `"`
    /// outright on any `cmd.exe`/`powershell` shim launch (an npm-installed
    /// `.cmd`, or a `.ps1`) -- so a codex pane spawned through a shim could
    /// never launch at all once `extra_writable_root_args` was wired in,
    /// unconditionally, into every dashboard-spawned worker pane. See
    /// `no_extra_writable_root_arg_ever_trips_the_cmd_shim_reparse_guard`
    /// below for the direct regression test.
    #[test]
    fn extra_writable_root_args_always_includes_the_mail_dir_but_never_the_bare_state_root() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(repo.path())
            .status()
            .expect("git init");
        let state_root = tempfile::tempdir().expect("tempdir");
        let mail_dir = state_root.path().join("mail");

        let adapter = CodexAdapter::new(None);
        let args = adapter.extra_writable_root_args(repo.path(), &mail_dir);

        let joined = args.join(" ");
        // Checked as a QUOTED, closed TOML string element (`"<path>"`), not a
        // bare substring: `mail_dir` is itself `<state_root>/mail`, so a
        // plain substring check for the state root's own path would also
        // match inside the (correct) mail entry. Quoting-and-closing is what
        // actually distinguishes "the state root named as its own array
        // element" from "the mail subtree, which happens to start with the
        // state root's path".
        //
        // Built through the same `toml_quoted_string` the implementation
        // uses, not a bare `"<path>"` wrap: `toml_quoted_string` quotes with
        // `'...'` (a TOML literal string) whenever it can, not `"..."`, so a
        // naive double-quote wrap never matches the real argv (found
        // 2026-08-26, Windows dev machine).
        let quoted_mail = toml_quoted_string(&mail_dir.display().to_string());
        let quoted_state_root = toml_quoted_string(&state_root.path().display().to_string());
        assert!(
            joined.contains(&quoted_mail),
            "must name the mail subtree: {args:?}"
        );
        assert!(
            !joined.contains(&quoted_state_root),
            "must never name the bare state root as its own writable-root entry: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c"
                    && w[1].starts_with("sandbox_workspace_write.writable_roots=[")),
            "got {args:?}"
        );
    }

    /// The other half of issue #119: a launch whose `cwd` is a linked `git
    /// worktree add` sibling must ALSO get the shared `.git` common dir as a
    /// writable root, since it sits outside `cwd` (and so outside `--sandbox
    /// workspace-write`'s own root) -- every git object/ref write from that
    /// worktree lands there. A real linked worktree, not a guessed path: the
    /// mechanism this pins is `git rev-parse --git-common-dir` itself, via
    /// the moved-and-shared `adapters::git_common_dir`.
    #[test]
    fn extra_writable_root_args_adds_the_shared_git_dir_for_a_linked_worktree() {
        let main_repo = tempfile::tempdir().expect("tempdir");
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(main_repo.path())
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "test"]);
        std::fs::write(main_repo.path().join("f.txt"), "x").expect("write");
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "init"]);

        let linked = tempfile::tempdir().expect("tempdir");
        let linked_path = linked.path().join("worktree");
        run_git(&["worktree", "add", linked_path.to_str().expect("utf8 path")]);

        let expected_git_dir =
            super::super::git_common_dir(main_repo.path()).expect("main repo has a git common dir");

        let state_root = tempfile::tempdir().expect("tempdir");
        let mail_dir = state_root.path().join("mail");
        let adapter = CodexAdapter::new(None);
        let args = adapter.extra_writable_root_args(&linked_path, &mail_dir);

        let joined = args.join(" ");
        // Same fix as `extra_writable_root_args_always_includes_the_mail_
        // dir_but_never_the_bare_state_root` above: compare against
        // `toml_quoted_string`'s own quoted form (a TOML literal string,
        // `'...'`), not a bare `.display()` substring check, which never
        // matches once the value is quoted.
        assert!(
            joined.contains(&toml_quoted_string(&expected_git_dir.display().to_string())),
            "must name the shared git common dir for a linked worktree: {args:?}"
        );
        assert!(
            joined.contains(&toml_quoted_string(&mail_dir.display().to_string())),
            "must still name the mail subtree too: {args:?}"
        );
    }

    /// The regression this round fixes, exercised end to end through the
    /// real guard rather than only inspecting `toml_quoted_string`'s output
    /// (2026-08-26): `extra_writable_root_args`'s own argv, appended after a
    /// `cmd.exe /c <shim>` prefix exactly as a real npm-installed codex shim
    /// launch would carry it, must never trip `guard_cmd_shim_reparse`. This
    /// is not a hypothetical -- `dash::worker_pane_extra_args` calls
    /// `extra_writable_root_args` unconditionally for every dashboard-spawned
    /// worker pane (see that function's own doc comment), so before this fix
    /// a codex pane reached through a `.cmd` shim could never launch at all.
    #[test]
    fn no_extra_writable_root_arg_ever_trips_the_cmd_shim_reparse_guard() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(repo.path())
            .status()
            .expect("git init");
        let state_root = tempfile::tempdir().expect("tempdir");
        let mail_dir = state_root.path().join("mail");

        let adapter = CodexAdapter::new(None);
        let extra = adapter.extra_writable_root_args(repo.path(), &mail_dir);
        assert!(!extra.is_empty(), "the mail root alone must still add args");

        let mut shim_args = vec!["/c".to_string(), "codex.cmd".to_string()];
        shim_args.extend(extra);
        crate::commands::ctx::adapters::guard_cmd_shim_reparse("cmd.exe", &shim_args)
            .expect("extra_writable_root_args must never trip the cmd-shim reparse guard");
    }

    /// Issue "codex approval hell" (2026-08-26): an operator's own `-c
    /// approval_policy=on-request` config override must pin the launch
    /// exactly the way a bare `--ask-for-approval on-request` flag already
    /// does -- `flags_pin_policy` now recognises the split `-c key=value`
    /// form (see its own doc comment in `adapters/mod.rs`), so
    /// `policy_launch_args` must append nothing at all after it, on any
    /// installed codex-cli regardless of what its own `--help` probes
    /// report.
    #[test]
    fn policy_launch_args_appends_nothing_after_an_operator_config_override_of_approval_policy() {
        let cfg = CtxConfig::default();
        let adapter = CodexAdapter::new(None)
            .with_on_request_approval_forced(true)
            .with_exec_ask_for_approval_forced(false);
        let flags = vec!["-c".to_string(), "approval_policy=on-request".to_string()];
        let out = crate::commands::ctx::adapters::policy_launch_args(
            &cfg,
            &adapter,
            &flags,
            super::super::LaunchMode::Headless,
        );
        assert!(
            out.is_empty(),
            "zirv must not append anything after the operator's own -c approval_policy=... \
             override: {out:?}"
        );
    }

    /// A multi-word agent bin (`ZIRV_CTX_AGENT_BIN="sh /tmp/stub.sh"`) must work
    /// the same way it does for claude, so `ZIRV_CTX_AGENT_BIN` behaves
    /// identically across adapters.
    #[test]
    fn a_multi_word_agent_bin_is_split_across_every_command_kind() {
        let adapter = CodexAdapter::new(Some("sh /tmp/stub.sh"));

        let headless = adapter.headless_cmd("go", &SessionId::parse("abc"), &[]);
        assert_eq!(headless.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = headless
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "exec".to_string(),
                "go".to_string()
            ],
            "the bin arguments come before the agent flags"
        );

        let interactive = adapter.interactive_cmd(Some("resume"), &[]);
        assert_eq!(interactive.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = interactive
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["/tmp/stub.sh".to_string(), "resume".to_string()]);
    }

    /// Codex nests rollouts under a date directory that `SessionRef` cannot
    /// predict (it carries only the id, not the session's start time), so
    /// `transcript_path` must scan for the id rather than compute the path.
    #[test]
    fn transcript_path_scans_the_dated_sessions_tree_for_the_session_id() {
        let home = tempfile::tempdir().expect("tempdir");
        let day_dir = home.path().join(".codex/sessions/2026/07/31");
        std::fs::create_dir_all(&day_dir).expect("mkdir");
        let expected =
            day_dir.join("rollout-2026-07-31T20-16-08-11111111-2222-4333-8444-555555555555.jsonl");
        std::fs::write(&expected, "").expect("write");

        let adapter = CodexAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(adapter.transcript_path(&session), expected);
    }

    /// `-m, --model <MODEL>` is verified on top-level `codex --help` too (see
    /// the `model_args` doc comment), so the dashboard's orchestrator pane
    /// can select a model on codex exactly as it does on claude.
    #[test]
    fn model_args_uses_the_verified_flag() {
        let adapter = CodexAdapter::new(None);
        assert_eq!(
            adapter.model_args("gpt-5.6-sol"),
            vec!["--model".to_string(), "gpt-5.6-sol".to_string()]
        );
    }

    /// SECURITY (FINDING 6, closed): an npm-installed `codex` on Windows is a
    /// `.cmd` shim, exactly the shape `ClaudeAdapter`'s own equivalent test
    /// (`a_cmd_shim_is_launched_through_cmd_exe_with_its_arguments_intact`)
    /// covers. `base()` must route it through `cmd.exe /c`, and
    /// `launches_through_cmd_shim` must report that shape so a caller moves
    /// the headless prompt onto stdin instead of leaving it on the reparsed
    /// argv.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_codex_is_launched_through_cmd_exe_and_reports_the_shim_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));
        assert!(
            adapter.launches_through_cmd_shim(),
            "a .cmd resolution must be reported as the shim shape"
        );

        let cmd = adapter.interactive_cmd(Some("resume this"), &["--last".to_string()]);
        assert!(
            cmd.get_program()
                .to_string_lossy()
                .to_lowercase()
                .contains("cmd"),
            "got {:?}",
            cmd.get_program()
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/c".to_string(),
                shim.display().to_string(),
                "resume this".to_string(),
                "--last".to_string(),
            ]
        );
    }

    /// FIX B, extended to codex: on the shim launch shape the headless prompt
    /// must never be the `exec <prompt>` argv token cmd.exe would reparse.
    /// `headless_cmd_stdin` omits the positional prompt entirely, relying on
    /// codex's own verified stdin fallback (`codex exec` with `[PROMPT]`
    /// omitted reads from stdin).
    #[test]
    fn headless_cmd_stdin_omits_the_prompt_and_reads_it_from_stdin() {
        let adapter = CodexAdapter::new(Some("/tmp/fake-codex"));
        let cmd = adapter
            .headless_cmd_stdin(
                &SessionId::parse("abc"),
                &["--model".to_string(), "gpt-5.6-luna".to_string()],
            )
            .expect("codex has a verified stdin form");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--model".to_string(),
                "gpt-5.6-luna".to_string(),
            ],
            "no positional prompt token: the prompt travels on stdin"
        );
    }

    /// A directly executable program (no `.cmd` extension, and not one that
    /// resolves to anything on `PATH` at all) is never the shim shape, off
    /// Windows or on it -- mirrors claude's own
    /// `a_program_is_spawned_exactly_as_written_off_windows` /
    /// `launches_through_cmd_shim` contract. Deliberately not the bare
    /// `"codex"` default: on a machine with a real npm-installed `codex.cmd`
    /// on `PATH`, that bare name legitimately *does* resolve to the shim
    /// shape, which is the behavior under test elsewhere in this file.
    #[test]
    fn a_direct_program_never_reports_the_shim_shape() {
        let adapter = CodexAdapter::new(Some("/tmp/fake-codex"));
        assert!(!adapter.launches_through_cmd_shim());
    }
}
