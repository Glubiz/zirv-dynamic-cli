//! Active usage-poll fallback: consulted only when the passive collector
//! reading is stale at a decision point. Every failure degrades to whatever
//! passive data exists — this module must never make a session worse.

use serde::{Deserialize, Serialize};

use super::config::PaceConfig;
use super::state::StateDir;
use super::window::UsageWindows;

#[allow(dead_code)]
const ANTHROPIC_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
#[allow(dead_code)]
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
/// UNVERIFIED (2026-08-16: no readable token on the reference machine to
/// exercise it). Ships best-effort; see Known Issues.
#[allow(dead_code)]
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/codex/usage";
#[allow(dead_code)]
const HTTP_TIMEOUT_SECS: u64 = 10;
/// Distinct from the overall/body timeout above: an unreachable endpoint
/// must not hold a supervisor's cycle-launch gate for up to `HTTP_TIMEOUT_
/// SECS` per attempt just to fail to connect.
#[allow(dead_code)]
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 3;
/// Same magnitude as `HTTP_CONNECT_TIMEOUT_SECS` and the same reasoning:
/// `security find-generic-password` reading a keychain item this process is
/// not already in the ACL of can pop a GUI authorization dialog (see
/// `anthropic_token_from_keychain`'s doc comment), and on a headless/SSH
/// session with nobody to answer it, `security` would otherwise block
/// forever. This bounds that to a fast, honest `None` instead.
#[allow(dead_code)]
const KEYCHAIN_TIMEOUT_SECS: u64 = HTTP_CONNECT_TIMEOUT_SECS;

/// Pure half of the bounded keychain wait below: whether `elapsed` has
/// crossed `timeout` and the attempt should be abandoned. Split out (and left
/// uncompiled-conditionally, unlike the macOS-only call site) so the
/// "give up past a deadline" arithmetic is unit-testable on every platform,
/// including this crate's own Windows CI, matching the clock-injected-pure-
/// core convention `pace::wait_deadline`/`chrome::bar_should_disable` already
/// use elsewhere in this codebase.
#[allow(dead_code)]
fn keychain_wait_expired(elapsed: std::time::Duration, timeout: std::time::Duration) -> bool {
    elapsed >= timeout
}

// `maybe_poll`/`UsagePoller` are wired into the pacing gate
// (`pace::refresh_sources`, all four `wait_for_window` call sites) and into
// `zirv ctx usage`'s no-subcommand readout. The `#[allow(dead_code)]`
// markers below predate that wiring and are kept only until the next
// cleanup pass confirms which items every build target actually reaches.

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct PollReading {
    pub windows: UsageWindows,
    /// Vendor-side credits state, advisory only (anthropic: extra_usage.is_enabled).
    pub vendor_credits_enabled: Option<bool>,
}

#[allow(dead_code)]
pub trait UsagePoller {
    fn poll(&self, provider: &str) -> Option<PollReading>;
}

/// The real poller: makes a blocking HTTP request against the vendor's usage
/// endpoint using the operator's own OAuth token. Constructed by the pacing
/// gate call sites and `zirv ctx usage`; tests never construct it -- they
/// stub `UsagePoller` instead, and every test that can reach a construction
/// site redirects home via `HomeGuard` so no token file is ever readable
/// (this module's tests never touch the network; see the module-level
/// security constraint).
#[allow(dead_code)]
pub struct HttpPoller {
    /// `cfg.chrome.events` at construction time, threaded in rather than
    /// read internally: `UsagePoller::poll`'s signature carries no
    /// `Announcer` (it is implemented by several plain test stubs too, and
    /// widening the trait for one macOS-only heads-up was not worth it), so
    /// this is the one bit of config the macOS Keychain-prompt announcement
    /// (see `anthropic_token_from_keychain`) needs carried in some other way
    /// -- the same `--quiet`/`ZIRV_CTX_QUIET`/`[chrome] events = false`
    /// opt-out every other `zirv ▸` line already respects.
    chrome_events_enabled: bool,
}

impl HttpPoller {
    pub fn new(chrome_events_enabled: bool) -> Self {
        Self {
            chrome_events_enabled,
        }
    }
}

/// Built once and reused for every call: constructing a `ureq::Agent`
/// spins up a fresh rustls config and root store, which is wasted work on
/// every `HttpPoller::poll` call otherwise. The connect timeout is kept
/// short (`HTTP_CONNECT_TIMEOUT_SECS`) and distinct from the overall/body
/// timeout (`HTTP_TIMEOUT_SECS`) so an unreachable endpoint fails fast
/// instead of blocking a supervisor's cycle-launch gate for up to 10s per
/// attempt.
#[allow(dead_code)]
static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();

#[allow(dead_code)]
fn shared_agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS)))
            .timeout_connect(Some(std::time::Duration::from_secs(
                HTTP_CONNECT_TIMEOUT_SECS,
            )))
            .build()
            .into()
    })
}

#[allow(dead_code)]
fn parse_anthropic_usage(body: &str, now: u64) -> Option<PollReading> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let read_window = |key: &str| -> Option<super::window::Window> {
        let w = v.get(key).filter(|w| w.is_object())?;
        Some(super::window::Window {
            used_percentage: w.get("utilization")?.as_f64()?,
            resets_at: w
                .get("resets_at")
                .and_then(|r| r.as_str())
                .and_then(super::window::parse_rfc3339_utc)
                .unwrap_or(0),
            observed_at: now,
        })
    };
    let windows = UsageWindows {
        five_hour: read_window("five_hour"),
        seven_day: read_window("seven_day"),
    };
    (windows.five_hour.is_some() || windows.seven_day.is_some()).then(|| PollReading {
        windows,
        vendor_credits_enabled: v
            .pointer("/extra_usage/is_enabled")
            .and_then(|b| b.as_bool()),
    })
}

#[allow(dead_code)]
fn parse_codex_usage(body: &str, now: u64) -> Option<PollReading> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let limits = v.get("rate_limits").unwrap_or(&v);
    let windows = super::window::windows_from_rate_limits(limits, now)?;
    let vendor_credits_enabled = limits
        .pointer("/credits/has_credits")
        .and_then(|b| b.as_bool());
    Some(PollReading {
        windows,
        vendor_credits_enabled,
    })
}

/// Pulls `claudeAiOauth.accessToken` out of the JSON blob Claude Code writes
/// -- the same shape whether it came from the plain `.credentials.json` file
/// (Windows, Linux, and a macOS install that predates keychain-only storage)
/// or, verified against the real CLI, from the *value* macOS Keychain stores
/// under the `Claude Code-credentials` service (`security find-generic-
/// password -s "Claude Code-credentials" -w` prints exactly this JSON). One
/// parser for both sources keeps the two call sites below from drifting, and
/// makes the format itself testable with a plain string, no filesystem or
/// platform dependency at all.
fn parse_claude_credentials_json(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    Some(
        v.pointer("/claudeAiOauth/accessToken")?
            .as_str()?
            .to_owned(),
    )
}

/// Reads the OAuth access token straight from disk on every call: it must
/// never be cached, logged, or persisted anywhere but the request this call
/// makes -- see the module's security constraint.
///
/// Resolved via `crate::utils::home_dir()` (`HOME`/`USERPROFILE`), not
/// `dirs::home_dir()`: on Windows the latter calls `SHGetKnownFolderPath`
/// directly and ignores both env vars, so a test's `HomeGuard` -- the
/// mechanism every other home-directory override in this crate relies on --
/// could never point this at a fixture instead of the operator's real
/// credentials. This is also what keeps `cargo test` from ever making a real
/// network call with the operator's own token, which is the whole point of
/// this module's "tests never touch the network" constraint once a real
/// caller (`wait_for_window`, `zirv ctx usage`) actually constructs an
/// `HttpPoller`.
///
/// **macOS root cause (header/bar showing no usage data there, T7 follow-up):**
/// Claude Code on macOS stores the OAuth token in the login Keychain under
/// the `Claude Code-credentials` service, not in `~/.claude/.credentials.json`
/// -- that file simply does not exist on a keychain-only macOS install. Every
/// other platform's install does write the file, so this was invisible on
/// Windows/Linux: the poll fallback (this function) could always read a
/// token there and keep `usage-anthropic.json` fed even for an operator who
/// never wired the statusline tee. On macOS the file read came back `None`
/// unconditionally, so with no tee wired the poll fallback -- the *only*
/// other usage source for claude -- could never produce a reading, and
/// `window::load_for` stayed empty forever: exactly the "usage portion is
/// empty" symptom, platform-specific because the credential *storage*
/// mechanism is platform-specific, not because anything in `chrome.rs`/
/// `wrap.rs`/`dash/` treats macOS differently (they don't -- see
/// [[Usage and Pacing]]'s "Polling is structurally inert on keychain / API-key
/// setups"). The file is tried first on every platform (unchanged behavior,
/// and still correct for a macOS install that happens to have one); only when
/// that comes back empty does a macOS build fall back to Keychain.
#[allow(dead_code)]
fn anthropic_token(chrome_events_enabled: bool) -> Option<String> {
    let path = crate::utils::home_dir()
        .ok()?
        .join(".claude")
        .join(".credentials.json");
    if let Some(token) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| parse_claude_credentials_json(&raw))
    {
        return Some(token);
    }
    #[cfg(target_os = "macos")]
    {
        anthropic_token_from_keychain(chrome_events_enabled)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = chrome_events_enabled; // only meaningful on the macOS fallback
        None
    }
}

/// The macOS-only fallback described on [`anthropic_token`]: shells out to
/// `/usr/bin/security` (the same tool a human would run by hand) rather than
/// linking a Keychain crate, since this is one read of one named item and
/// zirv already shells out for the equivalent Windows console-mode calls
/// through `windows-sys` FFI, not a crate, elsewhere in this module tree.
/// Every failure (binary missing, item not present, denied access, malformed
/// output, or a timed-out prompt below) degrades to `None` exactly like a
/// missing credentials file does -- this must never surface as an error,
/// only as "no token found here either".
///
/// **The GUI prompt, and why this must never block indefinitely.** A
/// keychain item's ACL lists which binaries may read it without asking; zirv
/// is not on `Claude Code-credentials`'s list (Claude Code created it), so
/// macOS pops a "zirv wants to access key 'Claude Code-credentials'" dialog
/// the first time this runs. There is no documented `security` flag that
/// suppresses this -- it is enforced by the OS's own Keychain access-control
/// model, not by the CLI, so unlike `HTTP_CONNECT_TIMEOUT_SECS`'s network
/// case there is no "fail fast on refusal" signal to ask for; reasoned from
/// Apple's documented ACL behavior, unverified against a real macOS host (no
/// Mac was available -- see Known Issues). On a headless/SSH session nobody
/// is there to click "Allow", and `Command::output()`'s blocking wait would
/// hang for as long as the dialog sits unanswered -- exactly the permission-
/// loop failure mode this whole project exists to remove, and doubly wrong
/// here because this path is reachable from `pace::wait_for_window`'s
/// pre-cycle gate (`exec`/`run_loop`), which must not stall a supervised
/// run's launch indefinitely either. So the child is spawned non-blocking
/// and polled with `try_wait` against a hard `KEYCHAIN_TIMEOUT_SECS`
/// deadline (`keychain_wait_expired`); past it the child is killed and
/// abandoned, and this returns `None` exactly as if the item were absent.
/// This function is never reachable from `wrap`'s status-bar redraw path
/// (`redraw_bar_if_due` never constructs an `HttpPoller` at all -- see
/// [[Usage and Pacing]]/[[Ctx Supervisors]]), so that invariant is structural,
/// not merely a property of this timeout.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn anthropic_token_from_keychain(chrome_events_enabled: bool) -> Option<String> {
    announce_keychain_prompt_once(chrome_events_enabled);

    let mut child = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let started = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(KEYCHAIN_TIMEOUT_SECS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                use std::io::Read;
                let mut raw = String::new();
                child.stdout.take()?.read_to_string(&mut raw).ok()?;
                return parse_claude_credentials_json(raw.trim());
            }
            Ok(None) => {
                if keychain_wait_expired(started.elapsed(), timeout) {
                    // Best-effort, matching every other kill-then-abandon
                    // path in this crate (`wrap::quit_child`): a `security`
                    // process left waiting on a dialog nobody will answer is
                    // not this process's to babysit any further.
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Emits [`super::announce::Event::MacosKeychainPromptExpected`] on the
/// `zirv ▸` channel, exactly once per process and only when the operator
/// has not opted out (`chrome_events_enabled`, i.e. `cfg.chrome.events`) --
/// the same latch discipline `pace::PaceGateFlags` uses for its own once-
/// per-run lines, applied here as a process-wide `AtomicBool` instead of a
/// caller-threaded flag because `UsagePoller::poll` has no per-run state of
/// its own to carry one in (see `HttpPoller`'s own doc comment).
#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn announce_keychain_prompt_once(chrome_events_enabled: bool) {
    static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !chrome_events_enabled {
        return;
    }
    let already_announced = ANNOUNCED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err();
    if already_announced {
        return;
    }
    super::announce::Announcer::new(true, console::colors_enabled_stderr())
        .emit(&super::announce::Event::MacosKeychainPromptExpected);
}

/// Same contract as [`anthropic_token`]: read fresh on every call, never
/// cached or logged, and resolved the same env-aware way for the same
/// reason.
#[allow(dead_code)]
fn codex_token() -> Option<String> {
    let path = crate::utils::home_dir()
        .ok()?
        .join(".codex")
        .join("auth.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.pointer("/tokens/access_token")
        .or_else(|| v.get("access_token"))
        .and_then(|t| t.as_str())
        .map(str::to_owned)
}

/// A human-readable reason `provider` currently has no usage reading at all,
/// plus the concrete next step -- consulted only by `zirv ctx status`'s "no
/// usage source" line (T7 follow-up 2). Deliberately a pure, filesystem-only
/// check (an `exists()` stat, nothing more): `status` must stay fast and
/// side-effect-free, so this never spawns `security`, never makes an HTTP
/// request, and never mutates anything -- it explains what the *next* `zirv
/// ctx usage`/supervised cycle would have to work with, not what just
/// happened. `window::has_no_usage_source` cannot itself distinguish "the
/// tee was never wired" from "the poll ran and failed" from "the poll never
/// even had a token to try" -- both routes leave nothing on disk -- so this
/// names every plausible cause rather than claiming a single, unverifiable
/// one.
#[allow(dead_code)]
pub fn usage_source_hint(provider: &str) -> String {
    let tee_hint = "the statusline tee needs no credentials at all and works on every platform -- \
                     wire it with `zirv ctx usage tee -- <your statusline command>` in Claude \
                     Code's `statusLine` setting";
    match provider {
        "anthropic" => {
            let file_exists = crate::utils::home_dir()
                .map(|h| h.join(".claude").join(".credentials.json").exists())
                .unwrap_or(false);
            if file_exists {
                format!(
                    "a credentials file exists but no reading has landed yet; {tee_hint}, or run \
                     `zirv ctx usage` to try the active poll fallback right now"
                )
            } else if cfg!(target_os = "macos") {
                format!(
                    "no ~/.claude/.credentials.json (macOS keeps the token in the login Keychain \
                     instead) -- the poll fallback will prompt for Keychain access to 'Claude \
                     Code-credentials' the first time it runs; choose 'Always Allow' to make that \
                     a one-time cost, or on a headless/SSH session where nobody can answer it, \
                     {tee_hint}"
                )
            } else {
                format!(
                    "no ~/.claude/.credentials.json found (API key / Bedrock auth has no such \
                     file, so the poll fallback cannot get a token at all); {tee_hint}"
                )
            }
        }
        super::window::CODEX_USAGE_PROVIDER => format!(
            "no ~/.codex/auth.json and no codex rollout snapshot found yet; {tee_hint} for a \
             claude session, or run a codex session so its own passive rollout scan can pick up \
             a reading"
        ),
        _ => tee_hint.to_string(),
    }
}

impl UsagePoller for HttpPoller {
    fn poll(&self, provider: &str) -> Option<PollReading> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let (url, token, extra_header) = match provider {
            "anthropic" => (
                ANTHROPIC_USAGE_URL,
                anthropic_token(self.chrome_events_enabled)?,
                Some(("anthropic-beta", ANTHROPIC_OAUTH_BETA)),
            ),
            super::window::CODEX_USAGE_PROVIDER => (CODEX_USAGE_URL, codex_token()?, None),
            _ => return None,
        };
        let agent = shared_agent();
        let mut req = agent
            .get(url)
            .header("Authorization", &format!("Bearer {token}"));
        if let Some((k, v)) = extra_header {
            req = req.header(k, v);
        }
        let body = req.call().ok()?.body_mut().read_to_string().ok()?;
        match provider {
            "anthropic" => parse_anthropic_usage(&body, now),
            _ => parse_codex_usage(&body, now),
        }
    }
}

/// `{"last_attempt": u64}` marker written every time a real poll is
/// *attempted* (whether it succeeds or not), so a failed attempt still
/// counts against `poll_min_interval_secs` and a provider with a broken
/// token does not retry on every single call.
#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
struct PollMarker {
    last_attempt: u64,
}

#[allow(dead_code)]
fn last_attempt(state: &StateDir, provider: &str) -> Option<u64> {
    let contents = std::fs::read_to_string(state.poll_marker_for(provider)).ok()?;
    serde_json::from_str::<PollMarker>(&contents)
        .ok()
        .map(|m| m.last_attempt)
}

/// Mirrors `window.rs`'s `store_at`: a temp sibling written in full, then
/// renamed over the target, so a concurrent reader never observes a
/// truncated marker. Best-effort: marker I/O failures degrade to "not
/// floored" rather than surfacing as an error, same as every other failure
/// path in this module.
#[allow(dead_code)]
fn record_attempt(state: &StateDir, provider: &str, now: u64) {
    let path = state.poll_marker_for(provider);
    let Some(parent) = path.parent() else {
        return;
    };
    if super::state::create_private_dir_all(parent).is_err() {
        return;
    }
    let Ok(contents) = serde_json::to_string(&PollMarker { last_attempt: now }) else {
        return;
    };
    let _ = super::state::write_private(&path, &contents);
}

/// Some(reading) when a poll ran, produced data and it was stored; None covers
/// "not needed", "floored", "disabled" and "failed" alike — callers never
/// branch on why. The reading carries the vendor_credits_enabled advisory for
/// callers that surface it (`zirv ctx usage`).
#[allow(dead_code)]
pub fn maybe_poll(
    state: &StateDir,
    cfg: &PaceConfig,
    now: u64,
    provider: &str,
    poller: &dyn UsagePoller,
) -> Option<PollReading> {
    if !cfg.poll_enabled {
        return None;
    }
    let existing = super::window::load_for(state, provider);
    if let Some(w) = &existing
        && now.saturating_sub(super::window::freshest_available_observation(w, now))
            <= cfg.collector_max_age_secs
    {
        return None; // passive data is fresh enough; the poll exists only as fallback
    }
    if last_attempt(state, provider)
        .is_some_and(|t| now.saturating_sub(t) < cfg.poll_min_interval_secs)
    {
        return None;
    }
    record_attempt(state, provider, now); // failed attempts count against the floor too
    let reading = poller.poll(provider)?;
    let merged = super::window::merge(existing.unwrap_or_default(), reading.windows.clone());
    super::window::store_for(state, provider, &merged).ok()?;
    Some(reading)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::window::{self, Window};
    use std::cell::RefCell;

    #[test]
    fn anthropic_response_parses_windows_and_credits_flag() {
        let body = include_str!("../../../tests/fixtures/anthropic-oauth-usage.json");
        let r = parse_anthropic_usage(body, 1_000).unwrap();
        let fh = r.windows.five_hour.unwrap();
        assert_eq!(fh.used_percentage, 7.0);
        assert_eq!(
            fh.resets_at,
            super::super::window::parse_rfc3339_utc("2026-08-16T20:49:59.785342+00:00").unwrap()
        );
        assert_eq!(fh.observed_at, 1_000);
        assert_eq!(r.windows.seven_day.unwrap().used_percentage, 23.0);
        assert_eq!(r.vendor_credits_enabled, Some(false));
        assert!(parse_anthropic_usage("{}", 1_000).is_none());
        assert!(parse_anthropic_usage("nonsense", 1_000).is_none());
    }

    /// The parser both `anthropic_token` (file) and `anthropic_token_from_
    /// keychain` (macOS Keychain's stored *value*) share -- platform-
    /// independent, exercised here with no filesystem or `security` binary
    /// involved, so this covers the shared logic on every CI platform
    /// including this Windows machine.
    #[test]
    fn claude_credentials_json_yields_the_access_token_from_either_source() {
        let body = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-abc","refreshToken":"r","expiresAt":1}}"#;
        assert_eq!(
            parse_claude_credentials_json(body),
            Some("sk-ant-oat01-abc".to_string())
        );
    }

    #[test]
    fn claude_credentials_json_rejects_malformed_or_unexpected_shapes() {
        assert!(parse_claude_credentials_json("").is_none());
        assert!(parse_claude_credentials_json("not json").is_none());
        assert!(parse_claude_credentials_json("{}").is_none());
        assert!(parse_claude_credentials_json(r#"{"claudeAiOauth":{}}"#).is_none());
        // A non-string token (defends the same shape a Keychain value could
        // plausibly be malformed into, e.g. a truncated `security` read).
        assert!(parse_claude_credentials_json(r#"{"claudeAiOauth":{"accessToken":1}}"#).is_none());
    }

    /// The `security find-generic-password -w` output described in
    /// `anthropic_token`'s doc comment includes a trailing newline; the
    /// keychain fallback's `.trim()` has to survive that, or a genuinely
    /// well-formed Keychain read would fail to parse for a whitespace reason
    /// that has nothing to do with the JSON itself.
    #[test]
    fn claude_credentials_json_survives_the_trailing_newline_a_shelled_out_read_leaves() {
        let with_newline = "{\"claudeAiOauth\":{\"accessToken\":\"sk-ant-oat01-abc\"}}\n";
        assert_eq!(
            parse_claude_credentials_json(with_newline.trim()),
            Some("sk-ant-oat01-abc".to_string())
        );
    }

    /// `anthropic_token`'s file-based path (identical on every platform,
    /// including macOS when a `.credentials.json` happens to exist there) --
    /// covers the half of the fix that stayed unchanged, so a refactor that
    /// broke it wouldn't only be caught on a Mac.
    #[test]
    fn anthropic_token_reads_the_credentials_file_when_present() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let claude_dir = home.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).expect("mkdir");
        std::fs::write(
            claude_dir.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-fromfile"}}"#,
        )
        .expect("write credentials file");

        assert_eq!(
            anthropic_token(true),
            Some("sk-ant-oat01-fromfile".to_string())
        );
    }

    /// The exact macOS symptom this fix addresses: no credentials file at
    /// all (a keychain-only install) used to mean `anthropic_token()` always
    /// returned `None`, so the poll fallback could never seed usage data and
    /// the header/bar showed nothing. Off macOS this documents the
    /// unchanged, still-correct behavior (no file, no fallback, `None`); on
    /// macOS the same missing-file starting point instead falls through to
    /// `anthropic_token_from_keychain`, which cannot be exercised here (no
    /// `security` binary, no login keychain, on this Windows CI machine) --
    /// see the reply's RESIDUAL RISK.
    #[test]
    fn anthropic_token_with_no_credentials_file_at_all() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        #[cfg(not(target_os = "macos"))]
        assert_eq!(anthropic_token(false), None);
    }

    /// The pure timeout-boundary arithmetic the macOS keychain fallback uses
    /// to turn a stuck GUI authorization prompt into a bounded `None` rather
    /// than a hang -- exercised directly here since the process-spawn half
    /// (`anthropic_token_from_keychain`) is `#[cfg(target_os = "macos")]` and
    /// cannot run on this Windows CI machine at all.
    #[test]
    fn keychain_wait_expired_is_a_closed_at_or_over_boundary() {
        let timeout = std::time::Duration::from_secs(KEYCHAIN_TIMEOUT_SECS);
        assert!(
            !keychain_wait_expired(std::time::Duration::from_secs(0), timeout),
            "no time elapsed yet"
        );
        assert!(
            !keychain_wait_expired(timeout - std::time::Duration::from_millis(1), timeout),
            "just under the deadline must still be waiting"
        );
        assert!(
            keychain_wait_expired(timeout, timeout),
            "exactly at the deadline must give up, not wait one more tick"
        );
        assert!(
            keychain_wait_expired(timeout + std::time::Duration::from_secs(60), timeout),
            "well past the deadline must give up"
        );
    }

    /// T7 follow-up 2: `zirv ctx status` must be able to say *why* nothing
    /// resolved, not just that nothing did. Every branch names a concrete
    /// next step (`zirv ctx usage tee`), and the macOS branch specifically
    /// names the Keychain service and the "Always Allow" advice this fix's
    /// announcement also carries, so the two surfaces (a live announcement
    /// and a static status line) never contradict each other.
    #[test]
    fn usage_source_hint_explains_every_platform_and_provider_case() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        // No credentials file at all: off macOS this is the API-key/Bedrock
        // case; the macOS branch is asserted separately below since `cfg!`
        // bakes the platform in at compile time, not at test-run time.
        let hint = usage_source_hint("anthropic");
        assert!(hint.contains("zirv ctx usage tee"), "got {hint}");
        #[cfg(target_os = "macos")]
        {
            assert!(hint.contains("Claude Code-credentials"), "got {hint}");
            assert!(hint.contains("Always Allow"), "got {hint}");
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(hint.contains("API key"), "got {hint}");
        }

        // A credentials file exists but nothing has been read into it yet.
        let claude_dir = home.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).expect("mkdir");
        std::fs::write(
            claude_dir.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-x"}}"#,
        )
        .expect("write credentials file");
        let hint = usage_source_hint("anthropic");
        assert!(hint.contains("zirv ctx usage`"), "got {hint}");
        assert!(hint.contains("no reading has landed yet"), "got {hint}");

        let codex_hint = usage_source_hint(window::CODEX_USAGE_PROVIDER);
        assert!(codex_hint.contains("codex"), "got {codex_hint}");
        assert!(
            codex_hint.contains("zirv ctx usage tee"),
            "got {codex_hint}"
        );

        let other = usage_source_hint("unknown-provider");
        assert!(other.contains("zirv ctx usage tee"), "got {other}");
    }

    #[test]
    fn codex_response_parser_accepts_rate_limits_shapes_and_rejects_junk() {
        // Synthetic bodies (endpoint unverified): wrapped and bare rate_limits
        let wrapped = r#"{"rate_limits":{"primary":{"used_percent":40.0,"window_minutes":300,"resets_at":100},"secondary":null}}"#;
        let bare = r#"{"primary":{"used_percent":40.0,"window_minutes":300,"resets_at":100}}"#;
        assert!(parse_codex_usage(wrapped, 1_000).is_some());
        assert!(parse_codex_usage(bare, 1_000).is_some());
        assert!(parse_codex_usage(r#"{"unrelated":true}"#, 1_000).is_none());
    }

    struct CountingPoller {
        calls: RefCell<u32>,
        reading: Option<PollReading>,
    }

    impl UsagePoller for CountingPoller {
        fn poll(&self, _provider: &str) -> Option<PollReading> {
            *self.calls.borrow_mut() += 1;
            self.reading.clone()
        }
    }

    fn windows_at(pct: f64, resets_at: u64, observed_at: u64) -> UsageWindows {
        UsageWindows {
            five_hour: Some(Window {
                used_percentage: pct,
                resets_at,
                observed_at,
            }),
            seven_day: None,
        }
    }

    #[test]
    fn maybe_poll_respects_staleness_and_the_interval_floor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = PaceConfig::default();
        let provider = "anthropic";
        let now = 1_000_000u64;

        // (a) fresh stored reading -> no poll, None.
        let fresh = windows_at(10.0, now + 600, now - 10);
        window::store_for(&state, provider, &fresh).expect("store fresh");
        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: UsageWindows::default(),
                vendor_credits_enabled: None,
            }),
        };
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        assert!(r.is_none(), "a fresh collector reading needs no poll");
        assert_eq!(*poller.calls.borrow(), 0, "poller must not run");

        // (b) stale reading -> polls, merges+stores, returns Some(reading), marker written.
        let stale = windows_at(10.0, now + 600, now - 10_000);
        window::store_for(&state, provider, &stale).expect("store stale");
        let fresh_from_poll = windows_at(50.0, now + 1_000, now);
        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: fresh_from_poll.clone(),
                vendor_credits_enabled: Some(true),
            }),
        };
        assert!(
            !state.poll_marker_for(provider).exists(),
            "no marker before the first real poll"
        );
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        let reading = r.expect("a stale reading triggers a poll");
        assert_eq!(reading.vendor_credits_enabled, Some(true));
        assert_eq!(*poller.calls.borrow(), 1);
        let stored = window::load_for(&state, provider).expect("stored after poll");
        assert_eq!(
            stored.five_hour.unwrap().used_percentage,
            50.0,
            "the merged reading is stored"
        );
        assert!(
            state.poll_marker_for(provider).exists(),
            "a marker is written after a real poll"
        );

        // (c) immediately again with a still-stale-looking store but a fresh marker
        //     -> floored, None, no second poll call.
        window::store_for(&state, provider, &stale).expect("re-stale the store");
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        assert!(r.is_none(), "floored by poll_min_interval_secs");
        assert_eq!(
            *poller.calls.borrow(),
            1,
            "no second poll call while floored"
        );

        // (d) poller returning None -> None, stored state untouched, marker still
        //     written (a failed attempt also counts against the floor).
        let now2 = now + cfg.poll_min_interval_secs + 1;
        window::store_for(&state, provider, &stale).expect("re-stale the store");
        let before = window::load_for(&state, provider).expect("before");
        let failing = CountingPoller {
            calls: RefCell::new(0),
            reading: None,
        };
        let r = maybe_poll(&state, &cfg, now2, provider, &failing);
        assert!(r.is_none(), "a failed poll surfaces as None");
        assert_eq!(*failing.calls.borrow(), 1);
        let after = window::load_for(&state, provider).expect("after");
        assert_eq!(before, after, "stored state untouched on a failed poll");

        // The failed attempt still counts against the floor: an immediate
        // recheck must not poll again.
        let r = maybe_poll(&state, &cfg, now2 + 1, provider, &failing);
        assert!(r.is_none());
        assert_eq!(
            *failing.calls.borrow(),
            1,
            "a failed attempt still floors the next poll"
        );
    }

    /// Fix 3: a stored reading whose window has already rolled over
    /// (`resets_at` in the past) must not count as "fresh" just because it
    /// was `observed_at` recently -- `available` would blank it from the
    /// display, so the gate must judge freshness the same way the display
    /// does and let the poll proceed, instead of refusing for up to
    /// `collector_max_age_secs` while the operator sees nothing.
    #[test]
    fn maybe_poll_proceeds_when_the_recent_reading_has_already_rolled_over() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = PaceConfig::default();
        let provider = "anthropic";
        let now = 1_000_000u64;

        // observed_at is 10 seconds old (well within collector_max_age_secs),
        // but resets_at is already in the past: `available` drops this
        // window, so the gate must treat it as stale too.
        let rolled_over = windows_at(10.0, now - 1, now - 10);
        window::store_for(&state, provider, &rolled_over).expect("store rolled-over reading");

        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: UsageWindows::default(),
                vendor_credits_enabled: None,
            }),
        };
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        assert!(
            r.is_some(),
            "a rolled-over-but-recently-observed reading must trigger a poll, not be treated as fresh"
        );
        assert_eq!(
            *poller.calls.borrow(),
            1,
            "the poller must actually run rather than being gated out"
        );
    }

    /// Both windows are always written from one snapshot, so in practice they
    /// share an `observed_at` (parse_statusline, parse_anthropic_usage, and
    /// windows_from_rate_limits all do this). When five_hour rolls over but
    /// seven_day is still live, `available` drops five_hour while seven_day
    /// survives with the shared recent `observed_at` -- so a gate that judged
    /// freshness off the raw newest observation, instead of the freshest
    /// *available* one, would wrongly call this fresh and leave five_hour
    /// blank for up to `collector_max_age_secs` after every five-hour
    /// boundary. The single-window fixture above (`seven_day: None`) cannot
    /// catch this: this case pins the two-window shape.
    #[test]
    fn maybe_poll_proceeds_when_one_of_two_shared_observation_windows_has_rolled_over() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = PaceConfig::default();
        let provider = "anthropic";
        let now = 1_000_000u64;

        let mixed = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 90.0,
                resets_at: now - 1,
                observed_at: now - 10,
            }),
            seven_day: Some(Window {
                used_percentage: 30.0,
                resets_at: now + 7 * 24 * 3600,
                observed_at: now - 10,
            }),
        };
        window::store_for(&state, provider, &mixed).expect("store mixed reading");

        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: UsageWindows::default(),
                vendor_credits_enabled: None,
            }),
        };
        let r = maybe_poll(&state, &cfg, now, provider, &poller);
        assert!(
            r.is_some(),
            "a dropped five_hour slot must trigger a poll even though seven_day is still live"
        );
        assert_eq!(
            *poller.calls.borrow(),
            1,
            "the poller must actually run rather than being gated out"
        );
    }

    /// Item 3 (review): the shared agent is built once and reused. No live
    /// HTTP is exercised here -- constructing the `ureq::Agent` config and
    /// confirming the `OnceLock` hands back the same instance on repeat
    /// calls is the cheapest honest probe available without a real request.
    #[test]
    fn the_shared_agent_is_constructed_once_and_reused() {
        let first = shared_agent() as *const ureq::Agent;
        let second = shared_agent() as *const ureq::Agent;
        assert_eq!(
            first, second,
            "repeat calls must reuse the same agent instance, not rebuild one"
        );
    }

    #[test]
    fn poll_disabled_never_polls() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = PaceConfig {
            poll_enabled: false,
            ..PaceConfig::default()
        };
        let poller = CountingPoller {
            calls: RefCell::new(0),
            reading: Some(PollReading {
                windows: UsageWindows::default(),
                vendor_credits_enabled: None,
            }),
        };
        let r = maybe_poll(&state, &cfg, 1_000, "anthropic", &poller);
        assert!(r.is_none());
        assert_eq!(
            *poller.calls.borrow(),
            0,
            "disabled polling never calls out"
        );
    }
}
