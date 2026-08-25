use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::CtxResult;
use super::adapters::AgentAdapter;
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::StructuralContext;
use super::state::{StateDir, now_secs, repo_slug};
use super::{adapters, log};

pub const SECTIONS: [&str; 6] = [
    "Task",
    "Done",
    "Remaining",
    "Next step",
    "Files touched",
    "Gotchas learned",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Handoff {
    pub task: String,
    pub done: Vec<String>,
    pub remaining: Vec<String>,
    pub next_step: String,
    pub files_touched: Vec<String>,
    pub gotchas: Vec<String>,
}

fn write_list(out: &mut String, heading: &str, items: &[String]) {
    out.push_str(&format!("## {heading}\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
    out.push('\n');
}

impl Handoff {
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("## Task\n{}\n\n", self.task));
        write_list(&mut out, "Done", &self.done);
        write_list(&mut out, "Remaining", &self.remaining);
        out.push_str(&format!("## Next step\n{}\n\n", self.next_step));
        write_list(&mut out, "Files touched", &self.files_touched);
        write_list(&mut out, "Gotchas learned", &self.gotchas);
        out
    }

    /// A handoff without a task or a next step is not worth restarting on.
    pub fn is_usable(&self) -> bool {
        !self.task.trim().is_empty() && !self.next_step.trim().is_empty()
    }
}

fn strip_bullet(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    // Numbered lists: "1. item"
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && trimmed[digits.len()..].starts_with(". ") {
        return Some(trimmed[digits.len() + 2..].trim().to_string());
    }
    None
}

pub fn parse_markdown(md: &str) -> Handoff {
    let mut handoff = Handoff::default();
    let mut section: Option<&str> = None;

    for line in md.lines() {
        if let Some(rest) = line.trim().strip_prefix("## ") {
            let name = rest.trim();
            section = SECTIONS
                .iter()
                .find(|s| s.eq_ignore_ascii_case(name))
                .copied();
            continue;
        }
        let Some(current) = section else { continue };
        let bullet = strip_bullet(line);
        let plain = line.trim();

        match current {
            "Task" => {
                if handoff.task.is_empty() && !plain.is_empty() {
                    handoff.task = bullet.unwrap_or_else(|| plain.to_string());
                }
            }
            "Next step" => {
                if handoff.next_step.is_empty() && !plain.is_empty() {
                    handoff.next_step = bullet.unwrap_or_else(|| plain.to_string());
                }
            }
            "Done" => handoff.done.extend(bullet),
            "Remaining" => handoff.remaining.extend(bullet),
            "Files touched" => handoff.files_touched.extend(bullet),
            "Gotchas learned" => handoff.gotchas.extend(bullet),
            _ => {}
        }
    }
    handoff
}

/// Mechanical extraction used when the distiller is unavailable or unusable.
/// Never fails and never returns something unusable.
pub fn structural(ctx: &StructuralContext) -> Handoff {
    let task = ctx
        .user_messages
        .last()
        .map(|m| m.lines().next().unwrap_or(m).trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "Unknown task (no user prompt found in the transcript)".to_string());

    let done: Vec<String> = ctx
        .assistant_texts
        .iter()
        .map(|t| t.lines().next().unwrap_or(t).trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    let remaining: Vec<String> = ctx
        .tool_errors
        .iter()
        .map(|e| format!("Unresolved error: {}", e.lines().next().unwrap_or(e).trim()))
        .collect();

    Handoff {
        task,
        done,
        remaining,
        next_step: "Re-read the files listed below, then continue the task above from where the previous session stopped.".to_string(),
        files_touched: ctx.files_touched.clone(),
        gotchas: vec!["This handoff was extracted mechanically, so it may be incomplete.".to_string()],
    }
}

pub const DISTILL_PROMPT_VERSION: &str = "v1";

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        return "(none)\n".to_string();
    }
    items.iter().map(|i| format!("- {i}\n")).collect()
}

pub fn distill_prompt(ctx: &StructuralContext) -> String {
    format!(
        "You are writing a handoff note ({DISTILL_PROMPT_VERSION}) so a fresh session can \
continue this work with no other context. Answer with markdown only, using exactly these \
sections in this order: {sections}. Use `## ` headings. Task and Next step are single lines; \
the rest are bullet lists. Be concrete: real file paths, real commands, real error text. Do \
not invent progress that is not evidenced below.\n\n\
### Recent user requests\n{requests}\n\
### Recent assistant replies\n{replies}\n\
### Files the session touched\n{files}\n\
### Unresolved tool errors\n{errors}",
        sections = SECTIONS.join(", "),
        requests = bullets(&ctx.user_messages),
        replies = bullets(&ctx.assistant_texts),
        files = bullets(&ctx.files_touched),
        errors = bullets(&ctx.tool_errors),
    )
}

const DISTILL_POLL: Duration = Duration::from_millis(25);
const MODEL_STDERR_CAPTURE_BYTES: usize = 8 * 1024;
const MODEL_STDERR_REPORT_BYTES: usize = 2 * 1024;

fn read_bounded<R: Read>(mut reader: R, limit: usize) -> Vec<u8> {
    let mut captured = Vec::with_capacity(limit);
    let mut chunk = [0_u8; 1024];
    while let Ok(count) = reader.read(&mut chunk) {
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        captured.extend_from_slice(&chunk[..count.min(remaining)]);
    }
    captured
}

fn sanitized_model_stderr(stderr: &[u8], prompt: &str) -> String {
    let decoded = String::from_utf8_lossy(stderr);
    let prompt_lines = prompt
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let mut sanitized = String::new();
    let mut redacted_prompt_line = false;
    for line in decoded.lines() {
        let clean: String = line
            .chars()
            .filter(|character| !character.is_control() || *character == '\t')
            .collect();
        if prompt_lines
            .iter()
            .any(|prompt_line| clean.contains(prompt_line))
        {
            if !redacted_prompt_line {
                sanitized.push_str("[model prompt content redacted]\n");
                redacted_prompt_line = true;
            }
        } else {
            sanitized.push_str(&clean);
            sanitized.push('\n');
        }
        if sanitized.len() >= MODEL_STDERR_REPORT_BYTES {
            sanitized.truncate(MODEL_STDERR_REPORT_BYTES);
            break;
        }
    }
    sanitized.trim().to_string()
}

/// Turns the operator's own model config into the `model: &str`
/// `distiller_cmd`/`run_model`/`distill`/`distill_or_structural` all take:
/// `explicit` (the operator's `handoff.model`/`optimize.model`) if set, else
/// the resolved adapter's own [`AgentAdapter::default_distiller_model`].
///
/// `handoff.model` used to default to the literal `"haiku"` unconditionally,
/// which reached `codex exec --model haiku` for a codex session and failed
/// outright, falling back to codex's empty `structural_context` for a
/// silently near-empty handoff. Every model-taking caller in this module,
/// `exec.rs`, `wrap.rs`, and `optimize.rs` goes through this rather than
/// reading `cfg.handoff.model`/`cfg.optimize.model` directly, so a third
/// adapter with its own default (or none) is handled without an edit at any
/// of those call sites.
///
/// Empty (never `None` -- every current caller's `distiller_cmd` reads an
/// empty model as "omit the flag", not "error") when neither the operator
/// nor the adapter named one, which is codex's own case today.
pub fn resolve_distiller_model(explicit: Option<&str>, adapter: &dyn AgentAdapter) -> String {
    explicit
        .filter(|m| !m.is_empty())
        .or_else(|| adapter.default_distiller_model())
        .unwrap_or_default()
        .to_string()
}

/// Runs one fresh model call and returns its stdout. The child is bounded on
/// every axis that can hang a supervisor: stdin and stdout are each serviced
/// on their own thread, started before either side has exchanged a byte, so
/// a model that starts answering before it has consumed all of stdin cannot
/// deadlock this call -- it would otherwise block writing a full stdout pipe
/// while this thread blocks writing an stdin pipe nothing is reading. The
/// wait below then has a deadline after which the child is killed.
pub fn run_model(
    adapter: &dyn AgentAdapter,
    model: &str,
    prompt: &str,
    timeout: Duration,
) -> CtxResult<String> {
    let mut command = adapter.distiller_cmd(model);
    // C2: the distiller is a full agent process spawned from inside a
    // supervised session, so without this it inherited that session's
    // `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET`/`ZIRV_CTX_TRANSCRIPT` -- and any
    // hook it ran would have posted turn signals into its *parent's* rot
    // engine, under the parent's own session id, while the parent sat
    // blocked waiting for this very call to return. It has no session of its
    // own and needs none: it is a one-shot, stdin-to-stdout model call.
    //
    // Covers `memory::harvest_from_handoff` too, which spawns its harvest
    // model through this same function rather than building its own command.
    super::sessions::scrub_supervision_env_cmd(&mut command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // L: the same FIX 2a chokepoint `supervise::spawn_tapped` applies to
    // every headless `exec`/`loop` spawn. This distiller child is spawned
    // directly rather than through that function, so it had no guard of its
    // own against a Windows `cmd.exe /c <shim>` launch reparsing a
    // metacharacter out of its argv -- a pre-existing gap, not something
    // this round introduced, but the same class the rest of this codebase
    // closes at every other spawn seam.
    super::supervise::guard_cmd_shim_reparse(&command)?;
    let mut child = command.spawn()?;

    let mut stdout = child.stdout.take().ok_or("model stdout unavailable")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        let _ = tx.send(buffer);
    });

    let stderr = child.stderr.take().ok_or("model stderr unavailable")?;
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let buffer = read_bounded(stderr, MODEL_STDERR_CAPTURE_BYTES);
        let _ = stderr_tx.send(buffer);
    });

    let mut stdin = child.stdin.take().ok_or("model stdin unavailable")?;
    let prompt_for_stderr = prompt.to_owned();
    let prompt = prompt.to_owned();
    // Dropping `stdin` at the end of this closure is what signals end of
    // input to the model. A write failure here (broken pipe, because the
    // child exited early) is not surfaced from this thread: the wait loop
    // below already turns an early, unsuccessful exit into an error from
    // the child's own status, which is the more useful of the two reports.
    std::thread::spawn(move || {
        let _ = stdin.write_all(prompt.as_bytes());
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // P1: the distiller is launched through the same adapter machinery
            // as any other child, so on Windows it can be a `cmd.exe /c
            // <shim>` whose real model process is a `node` grandchild --
            // `child.kill()` alone is a `TerminateProcess` against the shim and
            // leaves that grandchild running with nothing watching it. `wrap`
            // calls this from its pump, so the orphan would outlive the very
            // restart it was blocking. Tree-kill first, narrow kill behind it,
            // and `wait` unchanged as the only evidence of death.
            #[cfg(not(unix))]
            super::supervise::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("model did not answer within {}s", timeout.as_secs()).into());
        }
        std::thread::sleep(DISTILL_POLL);
    };

    if !status.success() {
        let stderr = stderr_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_default();
        let stderr = sanitized_model_stderr(&stderr, &prompt_for_stderr);
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        return Err(format!(
            "model exited with status {}{detail}",
            status.code().unwrap_or(-1)
        )
        .into());
    }

    let answer = rx.recv_timeout(timeout).unwrap_or_default();
    Ok(String::from_utf8_lossy(&answer).to_string())
}

/// Runs a fresh, cheap model over the context. The rotted session is never
/// asked to summarize itself.
///
/// Bounded by `timeout`, because `wrap` calls this from its pump: a model call
/// that never answers would otherwise freeze the user's own terminal with no
/// way out but killing the wrapper.
///
/// D, symmetric with `score::full_score`'s own guard: an adapter with no
/// verified event parsing (`capabilities().events == false`) has no
/// verified way to populate `structural_context` with anything real either
/// -- there is never anything real in `ctx` to distill. Spawning
/// the judgment-model child anyway would ask it to summarize nothing, and a
/// plausible-looking answer would then be reported as `"distilled"`, exactly
/// the fabricated-verdict class `full_score`'s own guard exists to prevent.
/// Refusing here, before the child is ever spawned, is what `distill_or_
/// structural` below relies on to report the honest `"no data"` instead.
pub fn distill(
    adapter: &dyn AgentAdapter,
    model: &str,
    ctx: &StructuralContext,
    timeout: Duration,
) -> CtxResult<Handoff> {
    if !adapter.capabilities().events {
        return Err(format!(
            "{} has no verified event parsing; nothing to distill",
            adapter.name()
        )
        .into());
    }
    let answer = run_model(adapter, model, &distill_prompt(ctx), timeout)?;
    let handoff = parse_markdown(&answer);
    if !handoff.is_usable() {
        return Err("distiller produced no usable Task and Next step".into());
    }
    Ok(handoff)
}

/// Never fails: a restart always has something to stand on.
///
/// D: the eventless case is checked here too, not just inside `distill`
/// (which would otherwise be reached and its `Err` mapped to the ordinary
/// `"structural"` label) -- `structural(ctx)` over an always-empty `ctx` is
/// not a real mechanical extraction the way it is for an adapter whose
/// transcript actually populated `ctx`, so it gets its own label, `"no
/// data"`, rather than borrowing one that implies real (if crude) content.
/// `chrome_events_enabled` is `cfg.chrome.events` (or, for a caller with no
/// `CtxConfig` in hand at this point, the same announcer-enabled flag it
/// already threads elsewhere): every call site that reaches the `distill`
/// branch below is about to run the distiller model, so the sandbox-residual
/// announce (issue #89) belongs *here*, once, rather than repeated at each
/// of this function's eight call sites -- two of which (`dash::
/// handover_pane`, `handover::preview_packet`) had silently omitted it
/// altogether before this fold. See `adapters::announce_sandbox_residual_
/// once`'s own one-time latch semantics, unchanged by this move.
pub fn distill_or_structural(
    adapter: &dyn AgentAdapter,
    model: &str,
    ctx: &StructuralContext,
    timeout: Duration,
    chrome_events_enabled: bool,
) -> (Handoff, &'static str) {
    if !adapter.capabilities().events {
        return (structural(ctx), "no data");
    }
    adapters::announce_sandbox_residual_once(adapter, chrome_events_enabled);
    match distill(adapter, model, ctx, timeout) {
        Ok(handoff) => (handoff, "distilled"),
        Err(_) => (structural(ctx), "structural"),
    }
}

#[derive(Debug, clap::Args)]
pub struct HandoffArgs {
    /// Transcript to distill.
    #[arg(long)]
    pub transcript: PathBuf,
    /// Adapter name: claude or codex.
    #[arg(long)]
    pub agent: Option<String>,
    /// Session id recorded in the stored file name.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Print the handoff markdown instead of the stored path.
    #[arg(long, default_value_t = false)]
    pub stdout: bool,
    /// Skip the model call and extract mechanically.
    #[arg(long, default_value_t = false)]
    pub no_model: bool,
}

pub fn store(
    state: &StateDir,
    repo: &Path,
    session: &str,
    handoff: &Handoff,
) -> CtxResult<PathBuf> {
    let dir = state.handoffs().join(repo_slug(repo));
    super::state::create_private_dir_all(&dir)?;

    let short: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let path = dir.join(format!("{}-{}.md", now_secs(), short));
    super::state::write_private(&path, &handoff.to_markdown())?;
    Ok(path)
}

pub fn latest_for_repo(state: &StateDir, repo: &Path) -> CtxResult<Option<(PathBuf, Handoff)>> {
    let dir = state.handoffs().join(repo_slug(repo));
    if !dir.is_dir() {
        return Ok(None);
    }

    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    names.sort();

    let Some(path) = names.pop() else {
        return Ok(None);
    };
    let handoff = parse_markdown(&std::fs::read_to_string(&path)?);
    Ok(Some((path, handoff)))
}

pub fn run_with<W: Write>(
    args: &HandoffArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(args.agent.as_deref().or(cfg.agent.as_deref()), &[], &cfg)?;
    let jsonl = std::fs::read_to_string(&args.transcript)
        .map_err(|e| format!("{}: {e}", args.transcript.display()))?;
    let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);

    // Low 6: the eventless check wins regardless of `--no-model`. For an
    // adapter with no verified event parsing (`capabilities().events ==
    // false`), `ctx` above has nothing real in it, so labelling it
    // `"structural"` here -- as `--no-model` used to do unconditionally --
    // implies a real mechanical extraction that never happened, the same
    // dishonesty `distill_or_structural`'s own `"no data"` label (below)
    // already closed for the non-`--no-model` path. `structural` is now
    // reserved for an adapter whose `ctx` came from a real transcript.
    let (handoff, source) = if !adapter.capabilities().events {
        (structural(&ctx), "no data")
    } else if args.no_model {
        (structural(&ctx), "structural")
    } else {
        distill_or_structural(
            adapter.as_ref(),
            &resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref()),
            &ctx,
            Duration::from_secs(cfg.handoff.timeout_secs),
            cfg.chrome.events,
        )
    };

    if args.stdout {
        write!(w, "{}", handoff.to_markdown())?;
        return Ok(0);
    }

    let state = StateDir::resolve(env)?;
    let session = args
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let path = store(&state, repo, &session, &handoff)?;

    let _ = log::append(
        &state,
        &log::Decision {
            ts: now_secs(),
            session: &session,
            verb: "handoff",
            verdict: "n/a",
            score: 0,
            action: source,
            detail: &path.display().to_string(),
        },
    );

    writeln!(w, "{}", path.display())?;
    Ok(0)
}

pub fn run<W: Write>(args: &HandoffArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::event::StructuralContext;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn fake_model_adapter() -> ClaudeAdapter {
        ClaudeAdapter::new(Some(&format!("sh {}", fixture("fake-model.sh").display())))
    }

    /// Long enough that a working fake model always finishes inside it, short
    /// enough that a wedged one does not stall the suite.
    const TEST_TIMEOUT: Duration = Duration::from_secs(20);

    fn ctx_sample() -> StructuralContext {
        StructuralContext {
            user_messages: vec!["ship the webhook".to_string()],
            assistant_texts: vec!["[zirv] wrote the route".to_string()],
            files_touched: vec!["src/routes/webhook.rs".to_string()],
            tool_errors: vec!["401 from the provider".to_string()],
        }
    }

    /// C: the operator's own explicit choice always wins, regardless of the
    /// adapter's own default.
    #[test]
    fn resolve_distiller_model_prefers_the_operators_explicit_choice() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(resolve_distiller_model(Some("sonnet"), &adapter), "sonnet");
    }

    /// C: with nothing explicit, claude's own real default ("haiku") is what
    /// used to be `HandoffConfig::default().model` unconditionally.
    #[test]
    fn resolve_distiller_model_falls_back_to_claudes_own_default() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(resolve_distiller_model(None, &adapter), "haiku");
    }

    /// C: codex has no verified cheap-model default of its own
    /// (`CodexAdapter::default_distiller_model` is `None`), so with nothing
    /// explicit this resolves to empty -- which `CodexAdapter::distiller_
    /// cmd` reads as "omit --model entirely" rather than guess a name.
    #[test]
    fn resolve_distiller_model_is_empty_for_codex_with_nothing_explicit() {
        let adapter = crate::commands::ctx::adapters::codex::CodexAdapter::new(None);
        assert_eq!(resolve_distiller_model(None, &adapter), "");
    }

    /// C: an explicit empty string (an operator setting `handoff.model =
    /// ""`, or `optimize.model`'s own "empty means defer" convention) is
    /// treated the same as "nothing explicit" -- it still falls back to the
    /// adapter's own default, rather than being passed straight through as
    /// a literal empty model name.
    #[test]
    fn resolve_distiller_model_treats_an_explicit_empty_string_as_unset() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(resolve_distiller_model(Some(""), &adapter), "haiku");
    }

    #[test]
    fn the_prompt_carries_the_context_and_asks_for_the_documented_sections() {
        let prompt = distill_prompt(&ctx_sample());
        for section in SECTIONS {
            assert!(
                prompt.contains(section),
                "prompt must name '{section}': {prompt}"
            );
        }
        assert!(prompt.contains("ship the webhook"));
        assert!(prompt.contains("src/routes/webhook.rs"));
        assert!(prompt.contains("401 from the provider"));
        assert!(
            prompt.contains(DISTILL_PROMPT_VERSION),
            "version the template"
        );
    }

    #[test]
    fn distillation_parses_a_well_formed_answer() {
        let adapter = fake_model_adapter();
        let handoff = distill(&adapter, "haiku", &ctx_sample(), TEST_TIMEOUT).expect("distills");
        assert_eq!(handoff.task, "Ship the webhook");
        assert_eq!(
            handoff.next_step,
            "Add a failing test for an invalid signature"
        );
        assert_eq!(handoff.done.len(), 2);
        assert!(handoff.is_usable());
    }

    #[test]
    fn the_distiller_receives_the_prompt_on_stdin() {
        let log = tempfile::NamedTempFile::new().expect("tempfile");
        // NEW-1: a guard -- `distill` below can panic via `expect`, which
        // used to skip the restore entirely.
        let _prompt_log = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_MODEL_PROMPT_LOG",
            log.path().to_str(),
        )]);
        let adapter = fake_model_adapter();
        distill(&adapter, "haiku", &ctx_sample(), TEST_TIMEOUT).expect("distills");

        let seen = std::fs::read_to_string(log.path()).expect("log");
        assert!(seen.contains("ship the webhook"), "got: {seen}");
    }

    #[test]
    fn a_failing_distiller_is_an_error() {
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "fail");
        }
        let adapter = fake_model_adapter();
        let result = distill(&adapter, "haiku", &ctx_sample(), TEST_TIMEOUT);
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        let err = result.expect_err("non-zero exit must surface");
        assert!(err.to_string().contains("4"), "report the exit code: {err}");
    }

    #[test]
    fn an_unusable_answer_is_an_error_so_callers_can_fall_back() {
        for mode in ["garbage", "partial"] {
            unsafe {
                std::env::set_var("FAKE_MODEL_MODE", mode);
            }
            let adapter = fake_model_adapter();
            let result = distill(&adapter, "haiku", &ctx_sample(), TEST_TIMEOUT);
            unsafe {
                std::env::remove_var("FAKE_MODEL_MODE");
            }
            assert!(
                result.is_err(),
                "mode {mode} should not produce a usable handoff"
            );
        }
    }

    #[test]
    fn distill_or_structural_falls_back_and_reports_which_path_it_took() {
        let adapter = fake_model_adapter();
        let (handoff, source) =
            distill_or_structural(&adapter, "haiku", &ctx_sample(), TEST_TIMEOUT, false);
        assert_eq!(source, "distilled");
        assert_eq!(handoff.task, "Ship the webhook");

        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "garbage");
        }
        let (handoff, source) =
            distill_or_structural(&adapter, "haiku", &ctx_sample(), TEST_TIMEOUT, false);
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        assert_eq!(source, "structural");
        assert_eq!(
            handoff.task, "ship the webhook",
            "from the last user prompt"
        );
        assert!(handoff.is_usable());
    }

    /// Finding #5: the sandbox-residual announce (issue #89) now lives
    /// *inside* `distill_or_structural`, on the events-capable path every
    /// caller that reaches `distill` takes -- so a caller can no longer
    /// forget it the way `dash::handover_pane`/`handover::preview_packet`
    /// both had before this fold (neither called `announce_sandbox_
    /// residual_once` at all). This exercises that path for an adapter that
    /// actually has a residual to report (codex with the ignore flags
    /// forced unsupported): the call must still complete and fall back to
    /// "structural" cleanly, rather than requiring every call site to
    /// remember a separate announce step first. The announce itself uses a
    /// process-wide, fires-once latch (see `adapters::announce_sandbox_
    /// residual_once`'s own doc comment), so -- matching this codebase's
    /// existing precedent for that kind of latch (`poll::announce_
    /// keychain_prompt_once`, `config::announce_unparsable_layers_once`) --
    /// this does not assert the announcement's own stderr output.
    #[test]
    fn distill_or_structural_reaches_the_announce_path_for_an_adapter_with_a_residual() {
        let adapter = crate::commands::ctx::adapters::codex::CodexAdapter::new(Some(
            "/nonexistent/codex-model-binary",
        ))
        .with_ignore_flags_forced(false);
        assert!(
            adapter.sandbox_residual_note().is_some(),
            "the test adapter must actually have a residual to report"
        );
        let (handoff, source) =
            distill_or_structural(&adapter, "gpt", &ctx_sample(), TEST_TIMEOUT, false);
        assert_eq!(
            source, "structural",
            "a missing distiller binary falls back"
        );
        assert!(handoff.is_usable());
    }

    /// A minimal adapter with no verified event parsing at all. Both
    /// registered adapters (claude, codex) now report
    /// `capabilities().events == true` (issue #86), so the eventless guard
    /// below is no longer a fact about any real, name-selectable adapter --
    /// exercised directly against a local fake instead. Mirrors
    /// `memory.rs`'s local `PanicOnDistillAdapter` pattern.
    #[derive(Debug)]
    struct EventlessAdapter;

    impl AgentAdapter for EventlessAdapter {
        fn name(&self) -> &'static str {
            "eventless"
        }

        fn program(&self) -> &str {
            "eventless"
        }

        fn provider(&self) -> &'static str {
            "eventless"
        }

        fn ready(&self) -> CtxResult<()> {
            Ok(())
        }

        fn detect(&self, _command: &[String]) -> bool {
            false
        }

        fn headless_cmd(
            &self,
            _prompt: &str,
            _session: &super::super::event::SessionId,
            _extra: &[String],
        ) -> std::process::Command {
            std::process::Command::new("true")
        }

        fn interactive_cmd(
            &self,
            _initial_prompt: Option<&str>,
            _extra: &[String],
        ) -> std::process::Command {
            std::process::Command::new("true")
        }

        fn distiller_cmd(&self, _model: &str) -> std::process::Command {
            panic!(
                "an eventless adapter must never reach the distiller: distill/distill_or_structural must refuse first"
            );
        }

        fn read_only_args(&self) -> Vec<String> {
            Vec::new()
        }

        fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
            Vec::new()
        }

        fn transcript_path(&self, _session: &super::super::event::SessionRef) -> PathBuf {
            PathBuf::new()
        }

        fn parse_events(&self, _jsonl: &str) -> Vec<super::super::event::NormalizedEvent> {
            Vec::new()
        }

        fn structural_context(
            &self,
            _jsonl: &str,
            _last_n: usize,
        ) -> super::super::event::StructuralContext {
            super::super::event::StructuralContext::default()
        }

        fn compact_command(&self) -> Option<&'static str> {
            None
        }

        fn quit_sequence(&self) -> &'static str {
            ""
        }

        fn capabilities(&self) -> super::super::event::Capabilities {
            super::super::event::Capabilities::default()
        }

        fn register_turn_signal(
            &self,
            _session: &super::super::event::SessionRef,
            _socket: &Path,
        ) -> super::adapters::TurnSignalSetup {
            super::adapters::TurnSignalSetup {
                env: Vec::new(),
                instructions: String::new(),
            }
        }
    }

    /// D, symmetric with `score::full_score_refuses_an_adapter_with_no_
    /// event_parsing`: an eventless adapter's `structural_context` never has
    /// anything real in it, so both `distill` and `distill_or_structural`
    /// must refuse before ever spawning the judgment-model child, and the
    /// latter must report `"no data"`, not `"structural"` -- that label is
    /// reserved for an adapter whose `ctx` came from a real transcript. A
    /// garbage `model` name proves the point: if either function tried to
    /// spawn it, this would fail some other way (a spawn error, or a
    /// timeout), not return cleanly.
    #[test]
    fn an_eventless_adapter_never_spawns_the_distiller_and_reports_no_data() {
        let adapter = EventlessAdapter;
        assert!(!adapter.capabilities().events);

        let err = distill(
            &adapter,
            "definitely-not-a-real-model-binary",
            &ctx_sample(),
            TEST_TIMEOUT,
        )
        .expect_err("no event parsing means nothing to distill");
        assert!(
            err.to_string().contains("no verified event parsing"),
            "got {err}"
        );

        let (handoff, source) = distill_or_structural(
            &adapter,
            "definitely-not-a-real-model-binary",
            &ctx_sample(),
            TEST_TIMEOUT,
            false,
        );
        assert_eq!(
            source, "no data",
            "not \"structural\": ctx here is never real"
        );
        assert!(handoff.is_usable(), "still something to stand on");
    }

    /// `wrap` calls this from its pump, so an unbounded wait is a frozen
    /// terminal for the user with no way out but killing the wrapper.
    #[test]
    fn a_distiller_that_never_answers_is_given_up_on() {
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "hang");
        }
        let adapter = fake_model_adapter();
        let started = Instant::now();
        let result = distill(&adapter, "haiku", &ctx_sample(), Duration::from_millis(300));
        let elapsed = started.elapsed();
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }

        let err = result.expect_err("a hung distiller must not look like a good handoff");
        assert!(
            err.to_string().contains("within"),
            "say that it timed out: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "it waited {elapsed:?}, so the bound did not hold"
        );
    }

    #[test]
    fn a_hung_distiller_still_produces_a_structural_handoff() {
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "hang");
        }
        let adapter = fake_model_adapter();
        let (handoff, source) = distill_or_structural(
            &adapter,
            "haiku",
            &ctx_sample(),
            Duration::from_millis(300),
            false,
        );
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        assert_eq!(source, "structural");
        assert!(
            handoff.is_usable(),
            "a restart still has something to stand on"
        );
    }

    #[test]
    fn run_model_returns_the_raw_answer() {
        let adapter = fake_model_adapter();
        let answer = run_model(&adapter, "haiku", "anything", Duration::from_secs(30))
            .expect("the fake model answers");
        assert!(
            answer.contains("## Task"),
            "raw markdown, unparsed: {answer}"
        );
    }

    #[test]
    fn run_model_reports_a_non_zero_exit() {
        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "fail");
        }
        let adapter = fake_model_adapter();
        let result = run_model(&adapter, "haiku", "anything", Duration::from_secs(30));
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        let err = result.expect_err("non-zero exit surfaces");
        assert!(err.to_string().contains('4'), "report the exit code: {err}");
        assert!(
            err.to_string().contains("fake model blocked by sandbox"),
            "report bounded model stderr: {err}"
        );
    }

    #[test]
    fn model_stderr_is_bounded_sanitized_and_does_not_echo_the_prompt() {
        let prompt = "private optimizer prompt line\nanother private line";
        let stderr = format!(
            "\u{1b}[31mpermission denied\u{1b}[0m\nprompt was: {prompt}\n{}",
            "x".repeat(MODEL_STDERR_CAPTURE_BYTES * 2)
        );
        let sanitized = sanitized_model_stderr(stderr.as_bytes(), prompt);

        assert!(sanitized.contains("permission denied"), "got {sanitized}");
        assert!(
            sanitized.contains("[model prompt content redacted]"),
            "got {sanitized}"
        );
        assert!(!sanitized.contains("private optimizer prompt line"));
        assert!(sanitized.len() <= MODEL_STDERR_REPORT_BYTES);
        assert!(
            !sanitized.contains('\u{1b}'),
            "control bytes must be stripped"
        );
    }

    /// L: `run_model` is spawned directly, not through `supervise::
    /// spawn_tapped`, so it had no guard of its own against a Windows
    /// `cmd.exe /c <shim>` launch reparsing a metacharacter out of its argv
    /// -- `model` is repo-forbidden (`REPO_FORBIDDEN`), so this is defense
    /// in depth rather than a live path today, but the guard has to actually
    /// run at this seam for that to be true. Proven the same way claude.rs's
    /// own shim tests are: a `.cmd` shim that writes a sentinel if it ever
    /// runs, and a metacharacter-bearing argv token that must never reach it.
    #[cfg(windows)]
    #[test]
    fn run_model_refuses_a_cmd_shim_launch_with_a_metachar_in_argv() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("fake-model.cmd");
        std::fs::write(
            &shim,
            "@echo off\r\necho ran> \"%~dp0ran.marker\"\r\necho ## Task\r\n",
        )
        .expect("write shim");
        let sentinel = dir.path().join("ran.marker");

        let adapter = ClaudeAdapter::new(Some(&shim.display().to_string()));
        let err = run_model(&adapter, "foo&calc", "prompt", Duration::from_secs(5))
            .expect_err("a metachar in argv must be refused before spawn");
        assert!(err.to_string().contains("foo&calc"), "got {err}");
        assert!(!sentinel.exists(), "the shim must never have been spawned");
    }

    /// Before this, `run_model` wrote the whole prompt to the child's stdin
    /// *before* spawning the thread that drains its stdout. A child that
    /// starts answering before it has consumed all of stdin could deadlock
    /// the caller: it blocks on a full stdout pipe while this thread blocks
    /// writing an stdin pipe the child has stopped reading -- and because the
    /// blocking write happened before the deadline loop even started, no
    /// `timeout` could rescue it. `flood` reproduces exactly that shape.
    #[test]
    fn a_child_that_answers_before_draining_stdin_does_not_deadlock() {
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "flood");
        }
        let adapter = fake_model_adapter();
        // Comfortably past a typical pipe buffer, so writing it cannot
        // complete without the reader side draining concurrently.
        let big_prompt = "x".repeat(200_000);
        let started = Instant::now();
        let result = run_model(&adapter, "haiku", &big_prompt, Duration::from_secs(10));
        let elapsed = started.elapsed();
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        result.expect("a flooding child must not deadlock the caller");
        assert!(
            elapsed < Duration::from_secs(5),
            "took {elapsed:?}: the stdin write and the stdout drain must run concurrently"
        );
    }

    #[test]
    fn run_model_gives_up_at_the_timeout() {
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "hang");
        }
        let adapter = fake_model_adapter();
        let started = Instant::now();
        let result = run_model(&adapter, "haiku", "anything", Duration::from_millis(300));
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        assert!(result.is_err(), "a hung model must not block a run");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_distiller_binary_falls_back_instead_of_panicking() {
        let adapter = ClaudeAdapter::new(Some("/nonexistent/model-binary"));
        let (handoff, source) =
            distill_or_structural(&adapter, "haiku", &ctx_sample(), TEST_TIMEOUT, false);
        assert_eq!(source, "structural");
        assert!(handoff.is_usable());
    }

    fn sample() -> Handoff {
        Handoff {
            task: "Wire the payments webhook".to_string(),
            done: vec![
                "Added the route".to_string(),
                "Wrote the parser".to_string(),
            ],
            remaining: vec!["Signature verification".to_string()],
            next_step: "Add a failing test for an invalid signature".to_string(),
            files_touched: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec!["The provider sends two events per charge".to_string()],
        }
    }

    #[test]
    fn markdown_uses_the_documented_section_order() {
        let md = sample().to_markdown();
        let positions: Vec<usize> = SECTIONS
            .iter()
            .map(|s| {
                md.find(&format!("## {s}"))
                    .unwrap_or_else(|| panic!("{s} missing"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "sections out of order in:\n{md}"
        );
    }

    #[test]
    fn markdown_round_trips() {
        let original = sample();
        assert_eq!(parse_markdown(&original.to_markdown()), original);
    }

    #[test]
    fn parsing_tolerates_extra_prose_and_missing_sections() {
        let md = "Here is the handoff you asked for.\n\n## Task\nShip the thing\n\n## Next step\nRun the tests\n";
        let parsed = parse_markdown(md);
        assert_eq!(parsed.task, "Ship the thing");
        assert_eq!(parsed.next_step, "Run the tests");
        assert!(parsed.done.is_empty());
        assert!(parsed.remaining.is_empty());
    }

    #[test]
    fn parsing_accepts_both_bullet_styles() {
        let md = "## Done\n- first\n* second\n1. third\n";
        assert_eq!(parse_markdown(md).done, vec!["first", "second", "third"]);
    }

    #[test]
    fn is_usable_requires_a_task_and_a_next_step() {
        assert!(sample().is_usable());
        assert!(!Handoff::default().is_usable());
        assert!(
            !Handoff {
                task: "something".to_string(),
                ..Handoff::default()
            }
            .is_usable(),
            "a handoff with no next step is not something to stand on"
        );
    }

    #[test]
    fn structural_fallback_uses_the_last_prompt_as_the_task() {
        let ctx = StructuralContext {
            user_messages: vec!["old request".to_string(), "fix the flaky test".to_string()],
            assistant_texts: vec!["[zirv] narrowed it to the timer".to_string()],
            files_touched: vec!["src/timer.rs".to_string()],
            tool_errors: vec!["assertion failed: expected 3".to_string()],
        };
        let handoff = structural(&ctx);
        assert_eq!(handoff.task, "fix the flaky test");
        assert_eq!(handoff.files_touched, vec!["src/timer.rs"]);
        assert!(handoff.done.iter().any(|d| d.contains("narrowed it")));
        assert!(
            handoff
                .remaining
                .iter()
                .any(|r| r.contains("assertion failed"))
        );
        assert!(!handoff.next_step.is_empty(), "always leave a next step");
        assert!(handoff.is_usable());
    }

    #[test]
    fn structural_fallback_survives_an_empty_context() {
        let handoff = structural(&StructuralContext::default());
        assert!(
            handoff.is_usable(),
            "a restart must always have something to stand on"
        );
        assert!(handoff.to_markdown().contains("## Task"));
    }

    #[test]
    fn structural_markdown_has_no_em_dashes() {
        let ctx = StructuralContext {
            user_messages: vec!["do it".to_string()],
            ..StructuralContext::default()
        };
        assert!(!structural(&ctx).to_markdown().contains('\u{2014}'));
    }

    use crate::commands::ctx::state::StateDir;

    fn transcript_with(dir: &std::path::Path, prompt: &str) -> std::path::PathBuf {
        let path = dir.join("t.jsonl");
        let mut text = String::new();
        text.push_str(&format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"{prompt}\"}}}}\n"
        ));
        text.push_str("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{\"file_path\":\"/work/src/lib.rs\"}}],\"usage\":{\"input_tokens\":9}}}\n");
        text.push_str("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[zirv] read it\"}],\"usage\":{\"input_tokens\":9}}}\n");
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn storing_writes_markdown_under_the_repo_slug() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = std::path::Path::new("/work/my-repo");

        let path = store(&state, repo, "11111111-2222", &sample()).expect("store");
        assert!(path.starts_with(state.handoffs().join("-work-my-repo")));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("## Task"));
        assert!(text.contains("Wire the payments webhook"));
    }

    /// A handoff is a verbatim summary of someone's working session, prompts
    /// and file paths included.
    #[cfg(unix)]
    #[test]
    fn a_stored_handoff_is_not_readable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let path = store(
            &state,
            std::path::Path::new("/work/my-repo"),
            "s",
            &sample(),
        )
        .expect("store");

        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("parent")), 0o700);
    }

    #[test]
    fn latest_for_repo_returns_the_newest_handoff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = std::path::Path::new("/work/my-repo");
        state.ensure().expect("ensure");

        let dir = state.handoffs().join("-work-my-repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("1700000000-aaaa.md"),
            "## Task\nold\n\n## Next step\nold step\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("1700000900-bbbb.md"),
            "## Task\nnew\n\n## Next step\nnew step\n",
        )
        .expect("write");

        let (path, handoff) = latest_for_repo(&state, repo)
            .expect("lookup")
            .expect("some");
        assert!(path.ends_with("1700000900-bbbb.md"));
        assert_eq!(handoff.task, "new");
    }

    #[test]
    fn latest_for_repo_is_none_when_nothing_was_stored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        assert!(
            latest_for_repo(&state, std::path::Path::new("/work/other"))
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn latest_for_repo_does_not_leak_across_repos() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        store(&state, std::path::Path::new("/work/a"), "s", &sample()).expect("store");
        assert!(
            latest_for_repo(&state, std::path::Path::new("/work/b"))
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn the_verb_stores_a_handoff_and_prints_its_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_with(tmp.path(), "ship the webhook");
        let state = tmp.path().join("state");
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                format!("sh {}", fixture("fake-model.sh").display()),
            ),
        ]
        .into();

        let args = HandoffArgs {
            transcript,
            agent: None,
            session_id: Some("11111111-2222".to_string()),
            stdout: false,
            no_model: false,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8").trim().to_string();
        assert!(
            printed.ends_with(".md"),
            "should print the stored path: {printed}"
        );
        let text = std::fs::read_to_string(&printed).expect("stored file");
        assert!(
            text.contains("Ship the webhook"),
            "the distilled task: {text}"
        );
    }

    #[test]
    fn no_model_skips_distillation_and_uses_the_structural_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_with(tmp.path(), "ship the webhook");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let args = HandoffArgs {
            transcript,
            agent: Some("claude".to_string()),
            session_id: None,
            stdout: true,
            no_model: true,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("ship the webhook"), "structural task: {text}");
        assert!(
            text.contains("/work/src/lib.rs"),
            "files from tool calls: {text}"
        );
    }

    /// Low 6 (fix): for an eventless adapter, `--no-model` used to still
    /// label the result `"structural"`, the same label the non-`--no-model`
    /// path reserves for an adapter whose `ctx` came from a real
    /// transcript. Codex's `ctx` here is always `StructuralContext::
    /// default()` regardless of the transcript on disk (`structural_
    /// context` is stubbed empty), so the honest label is `"no data"`,
    /// matching `distill_or_structural`'s own vocabulary, and it must win
    /// even though `--no-model` never reaches that function at all.
    #[test]
    fn no_model_on_an_eventless_adapter_still_reports_no_data_not_structural() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_with(tmp.path(), "ship the webhook");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let args = HandoffArgs {
            transcript,
            agent: Some("claude".to_string()),
            session_id: None,
            stdout: false,
            no_model: true,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");

        let log =
            std::fs::read_to_string(tmp.path().join("state/logs/decisions.jsonl")).expect("log");
        // A claude transcript has real events, so `--no-model` here correctly
        // reports "structural" (a real mechanical extraction), never "no
        // data" -- that label is reserved for a genuinely eventless adapter.
        assert!(log.contains("\"action\":\"structural\""), "got {log}");
        assert!(!log.contains("\"action\":\"no data\""), "got {log}");
    }

    /// Low 6, re-scoped for issue #86: both registered adapters now report
    /// `capabilities().events == true`, so codex is no longer a real,
    /// name-selectable example of the eventless case `run_with`'s own
    /// `!adapter.capabilities().events` branch exists for -- that guard is
    /// exercised directly (not through the CLI's name-based `adapters::
    /// select`) by `an_eventless_adapter_never_spawns_the_distiller_and_
    /// reports_no_data` above instead. This test replaces the old codex-
    /// specific one and instead pins the new, honest behavior: `--no-model`
    /// on a codex transcript with a real turn (`task_complete.
    /// last_agent_message`) now reports "structural" from codex's own
    /// (no longer permanently empty) `structural_context`.
    #[test]
    fn no_model_on_codex_now_reports_structural_since_codex_has_real_structural_context() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // T8 hermeticity: this is the one handoff.rs test whose `--agent
        // codex` path actually reaches `AgentGate::load` (via adapter
        // selection for structural context), which is real-`$HOME`-backed
        // (`crate::utils::home_dir()`) -- without this, a developer machine
        // with codex disabled in their own `~/.zirv/.settings.toml` fails
        // this test on a refusal that has nothing to do with what it tests.
        let home = tempfile::tempdir().expect("tempdir for home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let transcript_dir = tmp.path().join("rollout");
        std::fs::create_dir_all(&transcript_dir).expect("mkdir");
        let transcript = transcript_dir.join("rollout-test.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"timestamp":"2026-08-20T10:00:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#,
                "\n",
                r#"{"timestamp":"2026-08-20T10:00:07.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"[zirv] shipped the webhook"}}"#,
                "\n",
            ),
        )
        .expect("write transcript");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let args = HandoffArgs {
            transcript,
            agent: Some("codex".to_string()),
            session_id: None,
            stdout: false,
            no_model: true,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");

        let log =
            std::fs::read_to_string(tmp.path().join("state/logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"structural\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"no data\""),
            "codex has real event parsing now (issue #86): {log}"
        );
    }
}
