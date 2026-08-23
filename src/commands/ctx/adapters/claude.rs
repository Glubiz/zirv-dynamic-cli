use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::super::CtxResult;
use super::super::event::input_hash;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext, TranscriptUsage,
};
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

/// Claude Code's own base layer, injected on every claude session zirv starts
/// (see `AgentAdapter::base_system_prompt`). Claude-specific by construction:
/// it names the Agent tool, `.claude/agents` and the `/code-review` skill, so
/// handing it to another agent would be handing it instructions about tools
/// that agent does not have.
///
/// Names the Agent tool's own `model` parameter tiers (`haiku`/`sonnet`/
/// `opus`) directly, unlike the rest of this file's model-agnostic framing:
/// that parameter's enum is the harness's own fixed vocabulary, not a vendor
/// lineup that renames out from under this text, so naming it here is the
/// only way to make "set the model parameter explicitly" concrete enough to
/// follow. It still never asks for `--model`: that flag picks *this seat's*
/// model, which stays the operator's choice, untouched by this text.
pub const ORCHESTRATOR_PROMPT: &str = "\
zirv orchestrator conventions (claude)

You are an orchestrator. Coordination and judgment are the job, implementation is not: the \
orchestrator model is reserved for this seat, so delegate every substantive piece of work \
(codebase exploration, implementation, testing, review) to subagents via the Agent tool and keep \
your own replies brief and decision-focused.

- Bundle before you dispatch: never spawn an agent for one tiny task. Group small related tasks \
(same file or area, or a natural sequence) into a single checklist brief per agent, with a \
per-item output format. Split across agents only when tasks are independent and substantial, \
then dispatch them together and prefer background dispatch so a slow worker blocks nothing. For \
a small follow-up in an area a worker just handled, continue that worker instead of spawning a \
fresh one.
- Every Agent-tool dispatch MUST set the model parameter explicitly: haiku for mechanical and \
bulk work, sonnet for ordinary exploration, implementation and test writing, opus only for hard \
debugging and design exploration. Never omit it -- omission silently inherits this seat's own \
expensive model -- never dispatch on this seat's model, and never use fork-type subagents from \
this seat, which always inherit the seat model and ignore overrides. Agents in .claude/agents \
that pin their own model are exempt; do not override those, except the review-model rule below, \
which outranks this.
- Write self-contained briefs: state the goal, constraints, relevant file paths and exact output \
format, and nothing else -- subagents share none of your context. Every brief must itself tell \
the worker to reply briefly with compact structured findings, never raw file dumps.
- Decide rather than let a worker loop: choices between valid designs, architecture changes, and \
anything a worker has failed at twice come back to you. Do not read large files or write code \
yourself unless the change is trivial.
- Hold implementers to this repository's standards: follow the patterns already there, look for \
reusable code before adding new code, write a failing test first, keep diffs minimal, and run the \
project's format, lint and test commands before reporting back.
- Verify in batches: one independent reviewer gate per batch of related changes, not one per \
micro-task. You own the final integration, so resolve conflicts between agent outputs and report \
outcomes, including failures, plainly.
- Before reporting development work done, run this harness's own /code-review over the full diff \
at a single-reviewer effort level (low or medium), routed to the review model named in the \
harness roster, never this seat's own model, and never a high-or-above fan-out, which forks \
agents that inherit the seat's expensive model. A session that also carries the zirv meta-harness \
layer follows that layer's cross-harness review round on top.";

/// Claude's own layer for a delegated **Worker** session (see
/// `AgentAdapter::worker_system_prompt`), spliced in place of
/// [`ORCHESTRATOR_PROMPT`] for `PromptRole::Worker`. A worker never gets that
/// layer's coaching to delegate everything onward -- that would invite
/// recursion into a session that was itself already delegated to -- so this is
/// deliberately its own, much shorter text: execute the brief, do not spawn
/// further zirv workers, and report back plainly.
pub const WORKER_PROMPT: &str = "\
zirv worker conventions (claude)

You are a delegated worker session. Execute your brief directly and completely, then report \
compact results.

- Do not delegate onward: never run `zirv agent` or spawn further zirv workers; this task was \
already routed to you.
- If you use subagents for fan-out within your task, set each dispatch's model explicitly to the \
cheapest one that can do the job, never one above your own session's model, and never use \
fork-type subagents, which inherit this session's model and ignore overrides.
- Run code-review or verification passes only when your brief asks for them; the orchestrator that \
spawned you owns review rounds.
- Your final message is your report: lead with the outcome, keep it self-contained, and never dump \
raw file contents into it.";

fn text_of(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Real context size is `input_tokens` plus both cache fields; the bare
/// `input_tokens` field is near zero once prompt caching kicks in.
pub fn context_tokens_of(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
    .sum()
}

pub fn parse_events(jsonl: &str) -> Vec<NormalizedEvent> {
    let mut events = Vec::new();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let results: Vec<&Value> = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| {
                                b.get("type").and_then(Value::as_str) == Some("tool_result")
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                if results.is_empty() {
                    events.push(NormalizedEvent::TurnStart);
                    continue;
                }
                for block in results {
                    events.push(NormalizedEvent::ToolResult {
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
            }
            Some("assistant") => {
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let input_tokens = message.get("usage").map(context_tokens_of).unwrap_or(0);
                events.push(NormalizedEvent::AssistantFinal {
                    text: text_of(&message),
                    input_tokens,
                });

                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let raw = block.get("input").map(Value::to_string).unwrap_or_default();
                        events.push(NormalizedEvent::ToolCall {
                            name,
                            input_hash: input_hash(&raw),
                        });
                    }
                }
            }
            Some("system")
                if row.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                events.push(NormalizedEvent::Compaction);
            }
            _ => {}
        }
    }

    events
}

pub fn transcript_usage(jsonl: &str) -> Option<TranscriptUsage> {
    let mut usage = TranscriptUsage::default();
    let mut observed = false;
    for line in jsonl.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant")
            || row.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(current) = row.get("message").and_then(|message| message.get("usage")) else {
            continue;
        };
        observed = true;
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(context_tokens_of(current));
        usage.output_tokens = usage.output_tokens.saturating_add(
            current
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    }
    observed.then_some(usage)
}

const FILE_KEYS: &[&str] = &["file_path", "notebook_path", "path"];
const ERROR_SNIPPET: usize = 200;

pub fn structural_context(jsonl: &str, last_n: usize) -> StructuralContext {
    let mut out = StructuralContext::default();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let message = row.get("message").cloned().unwrap_or(Value::Null);

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let content = message.get("content");
                if let Some(text) = content.and_then(Value::as_str) {
                    out.user_messages.push(text.to_string());
                    continue;
                }
                let Some(blocks) = content.and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                out.user_messages.push(text.to_string());
                            }
                        }
                        Some("tool_result")
                            if block.get("is_error").and_then(Value::as_bool) == Some(true) =>
                        {
                            let detail = block
                                .get("content")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    block
                                        .get("content")
                                        .map(Value::to_string)
                                        .unwrap_or_default()
                                });
                            out.tool_errors
                                .push(detail.chars().take(ERROR_SNIPPET).collect());
                        }
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                let text = text_of(&message);
                if !text.trim().is_empty() {
                    out.assistant_texts.push(text);
                }
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                {
                    let Some(input) = block.get("input") else {
                        continue;
                    };
                    for key in FILE_KEYS {
                        if let Some(path) = input.get(*key).and_then(Value::as_str)
                            && !out.files_touched.iter().any(|p| p == path)
                        {
                            out.files_touched.push(path.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    keep_last(&mut out.user_messages, last_n);
    keep_last(&mut out.assistant_texts, last_n);
    keep_last(&mut out.tool_errors, last_n);
    // Capped with everything else rather than left to accumulate: this is a
    // deduplicated list of every path the whole session ever named, and it
    // leaves as a single argv token in a handoff. Windows caps a command line
    // at 32,767 characters, so an uncapped list is a long session that can no
    // longer relaunch at all.
    keep_last(&mut out.files_touched, last_n);
    out
}

fn keep_last(items: &mut Vec<String>, last_n: usize) {
    if items.len() > last_n {
        items.drain(..items.len() - last_n);
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    #[cfg(test)]
    forced_file_support: Option<bool>,
}

impl ClaudeAdapter {
    /// `bin` may carry arguments, so `"sh /tmp/stub.sh"` and
    /// `"/usr/bin/env claude"` both work. The first token is the program and the
    /// rest lead every command this adapter builds.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("claude").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "claude".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_file_support: None,
        }
    }

    /// Test seam: pins the home directory the transcript path is built from.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Test seam: bypasses the real `--help` probe so a unit test can force
    /// file-based or argv-based delivery without depending on the machine's
    /// installed binary.
    #[cfg(test)]
    pub fn with_file_support_forced(mut self, supported: bool) -> Self {
        self.forced_file_support = Some(supported);
        self
    }

    /// Every command starts here so the program and its leading arguments are
    /// applied uniformly to headless, interactive and distiller invocations,
    /// and so the Windows launcher rewrite (an npm-installed `claude` is
    /// `claude.cmd`, which `CreateProcess` refuses) is applied in exactly one
    /// place. A program zirv cannot resolve is spawned as written, which is
    /// today's behavior; `ready()` is where an unrunnable one is reported.
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

/// Bounds the `--help` probe below: a hang here must never hang the whole
/// launch, which would be a worse failure mode than falling back to argv.
const HELP_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Process-wide cache of `probe_system_prompt_file_support`'s answer, keyed
/// by the exact program invocation: the binary path plus its leading
/// arguments, since `ZIRV_CTX_AGENT_BIN` can point at different binaries (or
/// different versions of the same name resolved from a different PATH) and
/// each has its own answer. A restart inside the same `wrap`/`exec` run must
/// not re-spawn `--help` on every relaunch, which is what the cache is for;
/// it must not, in exchange, let one binary's probe answer for another's.
/// A tuple key rather than a joined string: joining `program` and `bin_args`
/// with spaces makes `("sh /tmp/x", ["--help"])` and `("sh", ["/tmp/x",
/// "--help"])` collide on the same string despite being different commands.
type ProbeKey = (PathBuf, Vec<String>);
static SYSTEM_PROMPT_FILE_SUPPORT: OnceLock<Mutex<HashMap<ProbeKey, bool>>> = OnceLock::new();

/// Probes the installed binary's own `--help` text for the file-based
/// system-prompt flag, so injection can move the composed prompt off argv
/// (visible to any other user on the machine via `ps`) without hard-coding a
/// version cutoff. Any failure to run or read the probe (binary missing,
/// timeout, whatever) is read as unsupported: this is a hardening on top of
/// argv delivery, never a new way to fail a launch.
fn probe_system_prompt_file_support(program: &str, bin_args: &[String]) -> bool {
    let key = (PathBuf::from(program), bin_args.to_vec());
    let cache = SYSTEM_PROMPT_FILE_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = cache.lock() else {
        return false;
    };
    if let Some(cached) = map.get(&key) {
        return *cached;
    }
    let detected = detect_help_flag(program, bin_args);
    map.insert(key, detected);
    detected
}

/// Verified against the real CLI (`claude --help`, v2.1.220): the flag is not
/// spelled out on its own line. It only appears folded into a shorthand,
/// `--append-system-prompt[-file]`, inside the `--bare` option's own
/// description ("Explicitly provide context via: --system-prompt[-file],
/// --append-system-prompt[-file], --add-dir ..."). Stripping `[` and `]`
/// before searching turns that shorthand into the plain flag text, and is a
/// no-op if some future help output ever spells the flag out on its own.
fn normalizes_to_advertise_the_file_flag(help_text: &str) -> bool {
    help_text
        .replace(['[', ']'], "")
        .contains("--append-system-prompt-file")
}

/// Runs `program --help` and reports whether its output names
/// `--append-system-prompt-file`. Stdin is nulled so an interactive TUI that
/// does not special-case `--help` (and just starts reading a line) gets an
/// immediate EOF instead of hanging the probe; stdout is drained on a
/// separate thread so a chatty `--help` cannot deadlock against the wait
/// loop by filling the pipe buffer.
fn detect_help_flag(program: &str, bin_args: &[String]) -> bool {
    // The same resolution the launch itself uses. Without it the probe and
    // the spawn disagree on Windows: `Command::new` only ever appends `.exe`,
    // so an npm-installed `claude.cmd` failed the probe here and then failed
    // the launch there, for two different reasons.
    let resolved =
        super::resolve_program(program).unwrap_or_else(|_| ResolvedProgram::direct(program));

    // SECURITY (FINDING 1): `bin_args` carries repo-controlled tokens on the
    // interactive path (`program_invocation` forwards every positional before
    // the first flag, e.g. `zirv chat --resume`'s handoff summary). When
    // `resolve_program` routes an npm-installed `claude.cmd` through
    // `cmd.exe /c <shim>`, cmd.exe reparses this whole probe command line, so a
    // metacharacter in `bin_args` would execute *here*, before the real launch
    // ever reaches its own `guard_cmd_shim_reparse`. Run the identical
    // fail-closed guard against the exact argv about to be spawned, and on a
    // rejection report "unsupported" (the same value every probe failure
    // yields) WITHOUT spawning -- the caller keeps argv delivery and the
    // payload is never executed.
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

    let deadline = Instant::now() + HELP_PROBE_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
            return normalizes_to_advertise_the_file_flag(&text);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Claude stores transcripts under a slug of the cwd with every character
/// outside `[A-Za-z0-9-]` replaced by `-`.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl AgentAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// Claude Code's subscription windows are Anthropic's, and the account is
    /// what the limit belongs to: a different Anthropic-backed harness would
    /// answer `"anthropic"` here too and share these readings.
    fn provider(&self) -> &'static str {
        "anthropic"
    }

    /// The one thing that can make this adapter unusable before it is asked
    /// to do anything: a program that resolves to a file this OS has no way
    /// to execute. Reported here, by name, rather than left to surface as a
    /// raw `os error 193` out of the spawn. A program that resolves to
    /// nothing at all is not an error here: that is the OS's own
    /// "not found", raised at spawn time where it has always been raised.
    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "claude")
            .unwrap_or(false)
    }

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        cmd
    }

    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        if prompt.trim().is_empty() {
            return Vec::new();
        }
        vec!["--append-system-prompt".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt")
    }

    fn system_prompt_file_flag(&self) -> Option<&'static str> {
        Some("--append-system-prompt-file")
    }

    fn base_system_prompt(&self) -> Option<&'static str> {
        Some(ORCHESTRATOR_PROMPT)
    }

    fn worker_system_prompt(&self) -> Option<&'static str> {
        Some(WORKER_PROMPT)
    }

    /// Counted over the argv the operator wrote, not over the argv `base()`
    /// builds: `exec` uses this to strip the program tokens off the command
    /// it was handed before carrying the rest into a restart. The Windows
    /// launcher rewrite lives entirely inside `base()` and never touches that
    /// argv, so the prefix stays the program plus its own leading arguments.
    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    fn supports_system_prompt_file(&self, launch: &[String]) -> bool {
        #[cfg(test)]
        if let Some(forced) = self.forced_file_support {
            return forced;
        }
        // The binary that is about to run, not the one this adapter would
        // have chosen: wrap spawns the user's own argv.
        let (program, args) = super::program_invocation(launch)
            .unwrap_or_else(|| (self.program.clone(), self.bin_args.clone()));
        probe_system_prompt_file_support(&program, &args)
    }

    /// True on a Windows npm install, where `claude` is a `.cmd` shim that
    /// [`super::resolve_program`] routes through `cmd.exe /c`. That is the one
    /// launch shape where a headless prompt on argv would be reparsed by
    /// cmd.exe, so on it the prompt is delivered via stdin instead
    /// (`headless_cmd_stdin`).
    fn launches_through_cmd_shim(&self) -> bool {
        super::launches_through_cmd_shim(&self.program)
    }

    /// The `-p` headless launch with **no positional prompt**: claude then
    /// reads the prompt from stdin (verified by the distiller, which does
    /// exactly this). Everything else matches `headless_cmd`, so a stdin
    /// launch and an argv launch differ only in where the prompt travels.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        Some(cmd)
    }

    /// The distillation prompt is piped to stdin so a long transcript tail
    /// never hits argv length limits. This child embeds untrusted repo
    /// CLAUDE.md text in its prompt (the judgment call) and its only job is
    /// to answer with text, so it never needs a tool. Verified against the
    /// real CLI (docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md,
    /// "I6 fix round"): `Bash` must be denied alongside `Write`/`Edit`, since
    /// a shell redirect otherwise recreates a Write tool, and the value must
    /// be one `=`-bound argv token, since the two-token form was verified to
    /// swallow the next argv entry.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--model")
            .arg(model)
            .arg("--output-format")
            .arg("text")
            .args(self.read_only_args());
        cmd
    }

    fn read_only_args(&self) -> Vec<String> {
        vec!["--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string()]
    }

    /// A real, verified cheap-model name for claude's own lineup -- the
    /// value `handoff.model`/`optimize.model` defaulted to before it became
    /// per-adapter (see `resolve_distiller_model` in `handoff.rs`). Specific
    /// to claude by construction: a hardcoded model name from one agent's
    /// lineup has no business leaking into another adapter's default.
    fn default_distiller_model(&self) -> Option<&'static str> {
        Some("haiku")
    }

    /// Claude's one verified per-run enforcement mechanism is the same
    /// `--disallowedTools=...` pin `distiller_cmd` above already relies on,
    /// probed against the real CLI (docs/superpowers/notes/2026-08-01-system-
    /// prompt-injection-facts.md). It names exactly four *tools*
    /// (`Write`/`Edit`/`Bash`/`NotebookEdit`), so it fully enforces exactly
    /// the two capabilities that pin denies outright:
    ///
    /// - **Repo filesystem writes** and **shell execution** -- `Write`/`Edit`
    ///   and `Bash` are two of the four denied tools, so a `Deny` stance is
    ///   `Enforced`.
    ///
    /// It answers everything else only partially or not at all:
    ///
    /// - **MCP/tool access** is `Degraded`, not `Enforced`: the pin denies
    ///   exactly `Write`/`Edit`/`Bash`/`NotebookEdit`, but `Read`, `Grep`,
    ///   `WebFetch`, `WebSearch`, `Task`, and every MCP server's own tools
    ///   remain available. Claiming `Enforced` here would claim a full tool
    ///   deny the pin does not deliver (docs/superpowers/notes/2026-08-01-
    ///   system-prompt-injection-facts.md:153).
    /// - **Approval** is `Unsupported`: the pin does not address approvals at
    ///   all. `WebFetch` domain approval and MCP tool approvals still prompt
    ///   even with all four tools denied, and `--permission-mode plan` was
    ///   probed and does not resolve in headless `-p` mode.
    /// - **Network** has no verified per-run flag at all.
    /// - **git push / destructive git** is reachable only through `Bash`, so
    ///   the only pin available denies *every* shell command, not git's --
    ///   over-broad enforcement that would break an ordinary session while
    ///   claiming to implement a git policy. Reported unsupported rather than
    ///   degraded so Task 14 cannot read this as "pin `--disallowedTools`".
    /// - **Writes outside the repo** are the same shape: no verified flag
    ///   scopes writes by path, and the available pin denies writes
    ///   everywhere, in-repo included.
    ///
    /// No stance reports as `Enforced` or `Degraded` at `Ask`: the pin can
    /// only deny outright, and claude's interactive ask-by-default comes
    /// from the operator's own settings, not from anything zirv pins.
    fn policy_support(
        &self,
        capability: crate::commands::ctx::policy::Capability,
        stance: crate::commands::ctx::policy::Stance,
    ) -> crate::commands::ctx::policy::CapabilityDescriptor {
        use crate::commands::ctx::policy::{Capability, CapabilityDescriptor, Stance};

        const TOOL_PIN: &str = "--disallowedTools=Write,Edit,Bash,NotebookEdit";
        const TOOL_PIN_PARTIAL: &str = "--disallowedTools=Write,Edit,Bash,NotebookEdit denies exactly those four \
             tools; Read, Grep, WebFetch, WebSearch, Task and every MCP server's own tools \
             remain available";
        const APPROVAL_UNSUPPORTED: &str = "the tool pin does not address approvals at all: WebFetch domain approval and \
             MCP tool approvals still prompt; `--permission-mode plan` was probed and does not \
             resolve in headless `-p` mode";
        const SETTINGS: &str = "claude's own permission prompts and `.claude/settings.json` permissions, which zirv \
             reads and never rewrites";

        match capability {
            Capability::RepoFsWrite | Capability::ShellExec => match stance {
                Stance::Deny => CapabilityDescriptor::enforced(TOOL_PIN),
                Stance::Ask | Stance::Allow => CapabilityDescriptor::operator_controlled(SETTINGS),
            },
            Capability::ToolAccess => match stance {
                Stance::Deny => CapabilityDescriptor::degraded(TOOL_PIN_PARTIAL),
                Stance::Ask | Stance::Allow => CapabilityDescriptor::operator_controlled(SETTINGS),
            },
            Capability::Approval => match stance {
                Stance::Deny => CapabilityDescriptor::unsupported(APPROVAL_UNSUPPORTED),
                Stance::Ask | Stance::Allow => CapabilityDescriptor::operator_controlled(SETTINGS),
            },
            // Network, git push/destructive git, and path-scoped writes --
            // see this method's own doc for why each is advisory rather than
            // carried by the pin above.
            Capability::Network
            | Capability::GitPushDestructive
            | Capability::OutsideRepoFsWrite => CapabilityDescriptor::advisory_only(),
        }
    }

    /// The one stance this adapter has a verified per-run mechanism for
    /// (`policy_support` above): `RepoFsWrite`/`ShellExec` at `Deny` gets the
    /// exact same `--disallowedTools=...` pin `read_only_args`/
    /// `distiller_cmd` already use -- reusing `self.read_only_args()`
    /// directly rather than a second literal keeps the two from ever drifting
    /// on the exact flag spelling the "I6 fix round" verified matters (see
    /// `distiller_cmd`'s own doc comment). Every other stance is
    /// `OperatorControlled` per `policy_support` and stays untouched: the
    /// shipped default (`EffectivePolicy::default()`, all `Allow`) returns
    /// empty, so a launch with no `[policy]` configured is byte-for-byte
    /// unaffected.
    fn policy_args(&self, policy: &crate::commands::ctx::policy::EffectivePolicy) -> Vec<String> {
        use crate::commands::ctx::policy::Stance;
        if policy.repo_fs_write == Stance::Deny || policy.shell_exec == Stance::Deny {
            self.read_only_args()
        } else {
            Vec::new()
        }
    }

    /// The claude side of the shipped-default "sandboxed, no prompts"
    /// posture (2026-08-22) -- verified against the actually-installed
    /// `claude 2.1.240` (`claude --help`, and confirmed at runtime against a
    /// real authenticated `-p` launch; both quoted in full in the
    /// 2026-08-22 addendum below and in [[Ctx Adapters]]). Claude has **no**
    /// real sandbox mechanism analogous to codex's `--sandbox
    /// workspace-write`: there is no flag that scopes writes/execution to
    /// the workspace while still allowing them freely. The two candidates
    /// that came closest were probed for real, not guessed:
    ///
    /// - `--dangerously-skip-permissions`/`bypassPermissions` removes the
    ///   permission system entirely -- explicitly excluded, per this fix's
    ///   own hard constraint: it satisfies "no prompts" only by also
    ///   satisfying "dangerous commands run", which this posture must never
    ///   do.
    /// - `--permission-mode acceptEdits` was probed live in headless `-p`
    ///   mode and, with no TTY to prompt through, silently **allowed**
    ///   both a `Write` and a destructive `rm <file>` `Bash` call with no
    ///   denial and no prompt -- effectively as permissive as the bypass
    ///   flag above in this launch shape. Disqualified for the same
    ///   reason.
    ///
    /// `--permission-mode dontAsk` is the one verified-safe match: probed
    /// live, it silently **denies** `Write`/`Bash` calls that are not
    /// pre-approved (`.claude/settings.json`'s own `permissions.allow`,
    /// which zirv reads and never writes) rather than prompting *or*
    /// running them, and its own embedded `--help` text (extracted from the
    /// installed binary) confirms this by design: `"'dontAsk' - Don't
    /// prompt for permissions, deny if not pre-approved."` This closes both
    /// halves of the posture's hard requirement (no prompts, nothing
    /// dangerous auto-runs), but `dontAsk` **alone**, with no pre-approved
    /// rules, is not "runs freely inside the workspace" -- it is inert: a
    /// legitimate in-repo `Write`/`Edit`/`Bash` action is denied outright.
    ///
    /// **Fix round 2 (2026-08-22): `SHIPPED_POSTURE_ALLOW`/`_DENY`**
    /// (`adapters/mod.rs`) is what makes `dontAsk` usable rather than merely
    /// safe -- generated `--allowedTools=...`/`--disallowedTools=...` argv,
    /// derived from that one shared list so this and codex's own posture
    /// cannot independently drift. Passed at launch, never written to
    /// `.claude/settings.json`: the operator's own file is untouched, and
    /// their own `permissions.allow`/`deny` there still governs anything
    /// this list is silent on. Verified live against the installed `claude
    /// 2.1.240` (see `SHIPPED_POSTURE_ALLOW`'s own doc comment for the
    /// specific findings -- `Edit(./**)` vs. bare `Write`, deny-over-allow
    /// precedence, prefix-wildcard semantics): an in-repo write succeeds
    /// with no prompt, a `cargo test` runs with no prompt, a write outside
    /// the workspace is refused, and `rm -rf` is refused even alongside a
    /// broader unrelated allow rule.
    ///
    /// **Fix round 3 (2026-08-22): `sandbox.extra_allow`/`extra_deny`**
    /// are appended after the shipped pair, not merged into it, so an
    /// operator's own addition can never silently replace a shipped entry --
    /// only add to either side. Deny still wins over allow regardless of
    /// which list (shipped or operator) an entry came from: both end up in
    /// the same `--allowedTools=`/`--disallowedTools=` argv, and the
    /// underlying CLI mechanism does not distinguish their origin.
    /// Projects `safety` (issue #83's harness-neutral command policy) onto
    /// claude's own `--allowedTools=`/`--disallowedTools=` vocabulary:
    /// `Read(./**)`/`Edit(./**)` are prepended directly (file-scope rules,
    /// not commands -- outside `[safety]`'s own domain, see `safety::
    /// command_pattern_from_bash_rule`'s doc comment), then every `safety`
    /// rule is re-wrapped as `Bash(<pattern>)`, then `sandbox.extra_allow`/
    /// `extra_deny` are appended last, unchanged from before this method
    /// took a `SafetyPolicy` parameter.
    ///
    /// Byte-identical to the pre-#83 hardcoded projection under the shipped
    /// default: `safety::builtin_deny`/`builtin_allow` strip exactly
    /// `SHIPPED_POSTURE_DENY`/`_ALLOW`'s own `Bash(...)` wrapper and this
    /// method re-adds it, so the round trip reproduces the original strings
    /// verbatim, in the original order -- pinned by
    /// `default_sandbox_args_stays_byte_identical_to_the_pre_safety_shipped_
    /// default` below.
    fn default_sandbox_args(
        &self,
        sandbox: &crate::commands::ctx::config::SandboxConfig,
        safety: &crate::commands::ctx::safety::SafetyPolicy,
    ) -> Vec<String> {
        let mut allow_entries: Vec<String> =
            vec!["Read(./**)".to_string(), "Edit(./**)".to_string()];
        allow_entries.extend(
            safety
                .allow
                .iter()
                .map(|rule| format!("Bash({})", rule.pattern)),
        );
        allow_entries.extend(sandbox.extra_allow.iter().cloned());
        let allow = allow_entries.join(",");

        let mut deny_entries: Vec<String> = safety
            .deny
            .iter()
            .map(|rule| format!("Bash({})", rule.pattern))
            .collect();
        deny_entries.extend(sandbox.extra_deny.iter().cloned());
        let deny = deny_entries.join(",");

        vec![
            "--permission-mode".to_string(),
            "dontAsk".to_string(),
            format!("--allowedTools={allow}"),
            format!("--disallowedTools={deny}"),
        ]
    }

    /// A delegated headless worker (`zirv ctx agent`, and the dashboard's
    /// own spawn-request pane variant) used to silently inherit whatever the
    /// operator's own interactive default model happened to be -- often a
    /// far pricier model than the delegated task actually needs. `"sonnet"`
    /// is the user-approved hard default that stops that, used only when
    /// the operator has not set `worker.claude` explicitly (see
    /// `adapters::resolve_worker_model`).
    fn default_worker_model(&self) -> Option<&'static str> {
        Some("sonnet")
    }

    /// Claude's own model ladder, top to bottom: `fable`/`mythos` (the
    /// orchestrator-tier aliases), `opus`, `sonnet`, `haiku`. Matched by
    /// substring on `seat`, lowercased first so `"claude-Opus-4-5"` and a
    /// bare `"opus"` both hit the same rung regardless of case (so
    /// `"claude-fable-5"` and a bare `"fable"` both hit the fable rung)
    /// rather than exact equality, since a seat string can carry a full id
    /// (`claude-opus-4-1`) or a bare alias. `haiku` is already the floor, so
    /// it maps to itself instead of falling off the ladder; an absent or
    /// unrecognised seat assumes the top tier, same as
    /// `AgentAdapter::review_model_below`'s own doc comment requires -- the
    /// deliberate consequence is that the computed default can then resolve
    /// to a model *more expensive* than the seat actually in use (an
    /// accepted spend-up default; the operator can override it with
    /// `[review]` or by setting `chat.model`).
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        let seat = seat.map(str::to_lowercase);
        match seat.as_deref() {
            Some(s) if s.contains("fable") || s.contains("mythos") => "opus",
            Some(s) if s.contains("opus") => "sonnet",
            Some(s) if s.contains("sonnet") => "haiku",
            Some(s) if s.contains("haiku") => "haiku",
            _ => "opus",
        }
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let projects = self.home_dir().join(".claude").join("projects");
        let computed = projects
            .join(project_slug(&session.cwd))
            .join(format!("{}.jsonl", session.id));
        if computed.exists() {
            return computed;
        }

        // The slug rule is verified for `/` and `.` but not every character,
        // so fall back to finding the session file wherever it landed.
        let wanted = format!("{}.jsonl", session.id);
        if let Ok(entries) = std::fs::read_dir(&projects) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(&wanted);
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        computed
    }

    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
        parse_events(jsonl)
    }

    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        structural_context(jsonl, last_n)
    }

    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        transcript_usage(jsonl)
    }

    fn compact_command(&self) -> Option<&'static str> {
        Some("/compact")
    }

    fn quit_sequence(&self) -> &'static str {
        "/exit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
            system_prompt: true,
            events: true,
        }
    }

    /// Verified against the real CLI (`claude --help`, v2.1.220): `--model
    /// <MODEL>` is a real flag.
    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_string(), model.to_string()]
    }

    /// `--resume <SESSION_ID>` is already a fact this codebase relies on
    /// elsewhere -- `wrap::extra_launch_flags` strips it (among the flags
    /// that pin a launch to an existing conversation) on every restart, and
    /// `exec.rs`'s own `RESUME_FLAGS_WITH_VALUE` treats it as a two-token
    /// flag -- so this is wiring up an already-verified flag for the
    /// dashboard's own restore path, not a fresh claim.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["--resume".to_string(), session_id.to_string()])
    }

    /// The same `--session-id <uuid>` flag `headless_cmd` already pins every
    /// headless run with (verified against the real CLI), offered here so an
    /// *interactive* dashboard pane can be pinned too. That is what makes the
    /// roster's stored uuid the claude conversation id, and therefore what
    /// makes `resume_args` above resolve to a real conversation after a quit.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        vec!["--session-id".to_string(), session.to_string()]
    }

    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: vec![
                (super::SOCKET_ENV.to_string(), socket.display().to_string()),
                (super::SESSION_ENV.to_string(), session.id.to_string()),
            ],
            instructions: "register a Stop hook running `zirv ctx hook stop` in \
                           ~/.claude/settings.json so turn boundaries reach the supervisor"
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::event::{NormalizedEvent, input_hash};

    pub(crate) fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// SECURITY (FINDING 1): `detect_help_flag` forwards `bin_args` (repo-
    /// controlled on the interactive path) into the `--help` probe argv. When
    /// the program resolves to the Windows `cmd.exe /c <shim>` form, cmd.exe
    /// would reparse a metacharacter in `bin_args` as a command -- so the probe
    /// must run the fail-closed `guard_cmd_shim_reparse` check *before* it
    /// spawns, and report "unsupported" (`false`) on rejection without ever
    /// executing anything. Proven by a shim that writes a sentinel next to
    /// itself if it ever runs: a metachar `bin_arg` leaves the sentinel absent,
    /// while a clean one spawns and creates it.
    #[cfg(windows)]
    #[test]
    fn detect_help_flag_refuses_to_spawn_when_a_bin_arg_would_be_reparsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        // `%~dp0` is the shim's own directory, so the sentinel lands there
        // regardless of the probe's cwd. If this batch file ever runs, the
        // sentinel exists.
        std::fs::write(&shim, "@echo off\r\necho ran> \"%~dp0ran.marker\"\r\n")
            .expect("write shim");
        let sentinel = dir.path().join("ran.marker");

        // A metacharacter-bearing bin_arg: the guard must refuse before spawn.
        let detected = detect_help_flag(&shim.display().to_string(), &["foo&calc".to_string()]);
        assert!(!detected, "a rejected probe reports unsupported");
        assert!(!sentinel.exists(), "the shim must never have been spawned");

        // Control: a clean bin_arg passes the guard and does spawn the shim
        // (which then creates the sentinel), confirming the guard -- not some
        // unrelated failure -- is what stopped the metachar case above.
        let _ = detect_help_flag(&shim.display().to_string(), &["--model".to_string()]);
        assert!(
            sentinel.exists(),
            "a clean bin_arg is allowed through and the shim runs"
        );
    }

    /// The needles track `scripts/record-claude-fixture.py`'s SECRET pattern.
    /// A scrub rule with no guard behind it is a rule that can silently stop
    /// working, and the cost of that is a credential in a public repository.
    #[test]
    fn recorded_fixture_carries_no_personal_data() {
        let text = std::fs::read_to_string(fixture_path("claude-real-session.jsonl"))
            .expect("fixture must be committed");
        for needle in [
            "jonathansolskov",
            "/Users/",
            "sk-ant",
            "sk-proj",
            "ghp_",
            "gho_",
            "ghu_",
            "ghs_",
            "ghr_",
            "AKIA",
            "-----BEGIN",
            "ApiKey ",
            "Bearer ",
            "eyJ",
        ] {
            assert!(
                !text.contains(needle),
                "fixture leaks '{needle}'; re-run scripts/record-claude-fixture.py"
            );
        }
        assert_eq!(
            credential_shape(&text),
            None,
            "fixture leaks a credential-shaped string; re-run scripts/record-claude-fixture.py"
        );
        assert!(
            text.contains("compact_boundary"),
            "fixture must include a compaction"
        );
        assert!(
            text.lines().count() >= 50,
            "fixture is too small to be representative"
        );
    }

    /// The two scrub rules a literal needle cannot express: the fixture
    /// legitimately contains `?key=REDACTED` and `checkout@v2`, so only the
    /// credential-shaped forms of each count as a leak.
    fn credential_shape(text: &str) -> Option<String> {
        for (index, _) in text.match_indices("key=") {
            let hex: String = text[index + 4..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            if hex.len() >= 8 {
                return Some(format!("key={hex}"));
            }
        }

        for (index, _) in text.match_indices('@') {
            let has_local = text[..index]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
            let domain: String = text[index + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            let tld = domain.rsplit('.').next().unwrap_or_default();
            let is_email = has_local
                && domain.contains('.')
                && tld.len() >= 2
                && tld.chars().all(|c| c.is_ascii_alphabetic());
            if is_email {
                return Some(format!("@{domain}"));
            }
        }
        None
    }

    #[test]
    fn the_credential_shape_check_separates_secrets_from_ordinary_text() {
        assert_eq!(
            credential_shape("http://localhost:1/?key=deadbeefcafe"),
            Some("key=deadbeefcafe".to_string())
        );
        assert_eq!(
            credential_shape("mail someone@example.com now"),
            Some("@example.com".to_string())
        );
        assert_eq!(credential_shape("?key=REDACTED"), None);
        assert_eq!(credential_shape("uses: actions/checkout@v2"), None);
        assert_eq!(credential_shape("#[cfg(test)] @testable import"), None);
    }

    #[test]
    fn context_tokens_sum_the_cache_fields() {
        // Verified against a real transcript: input_tokens alone is 2 in a
        // 110k-token session, so the cache fields carry the real size.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_creation_input_tokens": 457,
            "cache_read_input_tokens": 108_427,
            "output_tokens": 577
        });
        assert_eq!(context_tokens_of(&usage), 108_886);
    }

    #[test]
    fn transcript_usage_sums_actual_main_session_usage() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":5,"output_tokens":7}}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":100,"output_tokens":100}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":11,"output_tokens":13}}}"#,
        );
        assert_eq!(
            transcript_usage(jsonl),
            Some(TranscriptUsage {
                input_tokens: 21,
                output_tokens: 20,
            })
        );
        assert_eq!(transcript_usage("not json"), None);
    }

    #[test]
    fn a_real_prompt_starts_a_turn_but_a_tool_result_does_not() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"do the thing"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n"
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::TurnStart,
                NormalizedEvent::ToolResult { is_error: false },
            ]
        );
    }

    #[test]
    fn missing_is_error_counts_as_success() {
        let jsonl =
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(
            parse_events(jsonl),
            vec![NormalizedEvent::ToolResult { is_error: false }]
        );
    }

    #[test]
    fn assistant_yields_text_tokens_and_tool_calls() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":["#,
            r#"{"type":"thinking","thinking":"hmm"},"#,
            r#"{"type":"text","text":"[zirv] on it"},"#,
            r#"{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}"#,
            r#"],"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::AssistantFinal {
                    text: "[zirv] on it".to_string(),
                    input_tokens: 100,
                },
                NormalizedEvent::ToolCall {
                    name: "Bash".to_string(),
                    input_hash: input_hash("{\"command\":\"ls\"}"),
                },
            ]
        );
    }

    #[test]
    fn tool_only_assistant_messages_still_report_tokens() {
        let jsonl = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Read","input":{}}],"usage":{"input_tokens":5}}}"#;
        let events = parse_events(jsonl);
        assert_eq!(
            events[0],
            NormalizedEvent::AssistantFinal {
                text: String::new(),
                input_tokens: 5
            }
        );
    }

    #[test]
    fn compact_boundary_becomes_a_compaction_event() {
        let jsonl = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual"}}"#;
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::Compaction]);
    }

    #[test]
    fn sidechain_meta_and_garbage_lines_are_skipped() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","isMeta":true,"message":{"content":"hook noise"}}"#,
            "\n",
            "not json at all\n",
            "\n",
            r#"{"type":"pr-link","prNumber":7}"#,
            "\n",
            r#"{"type":"user","message":{"content":"real prompt"}}"#,
            "\n"
        );
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::TurnStart]);
    }

    /// The invariant the incremental scoring path rests on (see the
    /// `parse_events` contract on `AgentAdapter`): this parser is line-local,
    /// so a transcript cut at newlines and parsed piecewise yields exactly the
    /// events one parse of the whole file yields.
    #[test]
    fn parsing_a_transcript_in_pieces_yields_the_same_events() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let whole = parse_events(&jsonl);
        let lines: Vec<&str> = jsonl.lines().collect();

        for chunk in [1, 3, 17] {
            let pieced: Vec<NormalizedEvent> = lines
                .chunks(chunk)
                .flat_map(|piece| parse_events(&format!("{}\n", piece.join("\n"))))
                .collect();
            assert_eq!(pieced, whole, "in pieces of {chunk} lines");
        }
    }

    #[test]
    fn real_fixture_matches_recorded_expectations() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");

        let events = parse_events(&jsonl);
        let count = |pred: &dyn Fn(&NormalizedEvent) -> bool| {
            events.iter().filter(|e| pred(e)).count() as u64
        };
        let want = |key: &str| {
            expected[key]
                .as_u64()
                .unwrap_or_else(|| panic!("{key} missing"))
        };

        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::TurnStart)),
            want("turn_start")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::AssistantFinal { .. })),
            want("assistant")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolCall { .. })),
            want("tool_call")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: true })),
            want("tool_result_error")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: false })),
            want("tool_result_ok")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::Compaction)),
            want("compaction")
        );

        let last_tokens = events
            .iter()
            .rev()
            .find_map(|e| match e {
                NormalizedEvent::AssistantFinal { input_tokens, .. } => Some(*input_tokens),
                _ => None,
            })
            .expect("fixture has assistant events");
        assert_eq!(last_tokens, want("last_context_tokens"));
        assert!(
            want("tool_result_error") >= 1,
            "fixture must contain a tool error"
        );
    }

    use crate::commands::ctx::adapters::{AgentAdapter, SESSION_ENV, SOCKET_ENV};
    use crate::commands::ctx::event::{SessionId, SessionRef};

    /// I: `super::super::built_args` (`adapters/mod.rs`) takes the program
    /// string rather than the whole adapter, since `program` is private to
    /// this module -- this thin wrapper is what lets every call site below
    /// keep passing `&adapter` unchanged.
    fn built_args(adapter: &ClaudeAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(&adapter.program, cmd)
    }

    #[test]
    fn project_slug_matches_on_disk_evidence() {
        assert_eq!(
            project_slug(std::path::Path::new(
                "/Users/x/Documents/Privat/zirv-fitness-tracking"
            )),
            "-Users-x-Documents-Privat-zirv-fitness-tracking"
        );
        // A dot becomes a dash, which is why worktrees show up as `--claude-worktrees`.
        assert_eq!(
            project_slug(std::path::Path::new("/Users/x/repo/.claude-worktrees/b")),
            "-Users-x-repo--claude-worktrees-b"
        );
    }

    #[test]
    fn transcript_path_is_derived_from_home_and_cwd() {
        let home = tempfile::tempdir().expect("tempdir");
        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(
            adapter.transcript_path(&session),
            home.path()
                .join(".claude/projects/-work-repo/11111111-2222-4333-8444-555555555555.jsonl")
        );
    }

    #[test]
    fn transcript_path_falls_back_to_scanning_when_the_slug_misses() {
        let home = tempfile::tempdir().expect("tempdir");
        let real = home.path().join(".claude/projects/some-other-slug");
        std::fs::create_dir_all(&real).expect("mkdir");
        let actual = real.join("11111111-2222-4333-8444-555555555555.jsonl");
        std::fs::write(&actual, "").expect("write");

        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(adapter.transcript_path(&session), actual);
    }

    #[test]
    fn headless_cmd_pins_the_session_id() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let cmd = adapter.headless_cmd(
            "do the work",
            &SessionId::parse("abc"),
            &["--model".to_string(), "sonnet".to_string()],
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "/tmp/fake-claude");
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "do the work".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ]
        );
    }

    /// FIX B: the stdin headless form keeps `-p` and the session pin but never
    /// puts the prompt on argv -- claude reads it from stdin instead, so a
    /// prompt bearing a cmd.exe metacharacter is never reparsed on the shim
    /// form. The extra flags (the file-based system prompt, the operator's own)
    /// still ride on argv, exactly as `headless_cmd` places them.
    #[test]
    fn headless_cmd_stdin_omits_the_prompt_and_reads_it_from_stdin() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let cmd = adapter
            .headless_cmd_stdin(
                &SessionId::parse("abc"),
                &[
                    "--append-system-prompt-file".to_string(),
                    "/s/p.md".to_string(),
                ],
            )
            .expect("claude has a verified stdin form");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--append-system-prompt-file".to_string(),
                "/s/p.md".to_string(),
            ],
            "no positional prompt token: the prompt travels on stdin"
        );
        assert!(
            !args.iter().any(|a| a.contains("foo") || a.contains('&')),
            "the prompt text is nowhere in argv: {args:?}"
        );
    }

    #[test]
    fn interactive_cmd_passes_the_initial_prompt_positionally() {
        let adapter = ClaudeAdapter::new(None);
        let with = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(built_args(&adapter, &with), vec!["resume this".to_string()]);

        let without = adapter.interactive_cmd(None, &["--continue".to_string()]);
        assert_eq!(
            built_args(&adapter, &without),
            vec!["--continue".to_string()]
        );
    }

    #[test]
    fn distiller_cmd_uses_a_cheap_model_and_reads_stdin() {
        let adapter = ClaudeAdapter::new(None);
        let cmd = adapter.distiller_cmd("haiku");
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "-p".to_string(),
                "--model".to_string(),
                "haiku".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
                "--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string(),
            ]
        );
    }

    /// I6: the judgment/distiller child embeds untrusted repo CLAUDE.md text
    /// in its prompt and otherwise runs with the operator's full tool
    /// permissions. Verified against the real CLI (see
    /// docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md):
    /// this is the one flag, in the one argv shape, that provably blocks
    /// tool use, including an adversarial attempt to route around it via
    /// Bash or Task delegation.
    #[test]
    fn the_distiller_denies_the_tools_verified_to_matter() {
        let adapter = ClaudeAdapter::new(None);
        let cmd = adapter.distiller_cmd("haiku");
        let args = built_args(&adapter, &cmd);

        let deny = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("the distiller must restrict its own tools");
        assert_eq!(
            deny, "--disallowedTools=Write,Edit,Bash,NotebookEdit",
            "must be one argv token (a two-token --flag value form was \
             verified to swallow the next argv entry instead): {args:?}"
        );
        assert!(
            deny.contains("Bash"),
            "Bash alone bypasses a Write/Edit-only deny list via a shell \
             redirect, verified against the real CLI: {deny}"
        );
    }

    /// Bug B (harness parity): the shipped default `[policy]` (all `Allow`)
    /// must leave a real launch byte-for-byte unaffected -- `policy_args` is
    /// new, but an operator who never touched `[policy]` must see no argv
    /// change at all.
    #[test]
    fn policy_args_is_empty_under_the_default_all_allow_policy() {
        let adapter = ClaudeAdapter::new(None);
        assert!(
            adapter
                .policy_args(&crate::commands::ctx::policy::EffectivePolicy::default())
                .is_empty()
        );
    }

    /// A `[policy] shell_exec = "deny"` (or `repo_fs_write = "deny"`) must
    /// reach a real launch as the exact same `--disallowedTools=...` pin the
    /// distiller already relies on -- reusing `read_only_args()` rather than
    /// a second literal is what guarantees the two can never drift.
    #[test]
    fn policy_args_pins_the_verified_tool_deny_when_shell_exec_is_denied() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = ClaudeAdapter::new(None);
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        assert_eq!(
            adapter.policy_args(&policy),
            adapter.read_only_args(),
            "policy_args must reuse read_only_args verbatim, never a second literal"
        );
    }

    #[test]
    fn policy_args_pins_the_verified_tool_deny_when_repo_fs_write_is_denied() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = ClaudeAdapter::new(None);
        let policy = EffectivePolicy {
            repo_fs_write: Stance::Deny,
            ..EffectivePolicy::default()
        };
        assert_eq!(adapter.policy_args(&policy), adapter.read_only_args());
    }

    /// `Ask` stays `OperatorControlled` (see `policy_support`): claude has no
    /// verified per-run mechanism for it, so `policy_args` must not invent
    /// one.
    #[test]
    fn policy_args_leaves_ask_untouched() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let adapter = ClaudeAdapter::new(None);
        let policy = EffectivePolicy {
            shell_exec: Stance::Ask,
            ..EffectivePolicy::default()
        };
        assert!(adapter.policy_args(&policy).is_empty());
    }

    /// The shipped-default posture (2026-08-22): verified against the real
    /// installed binary (see this method's own doc comment) as the one
    /// claude mode that both suppresses prompts and never auto-runs an
    /// unapproved action -- `bypassPermissions`/`acceptEdits` were probed
    /// and rejected for doing the opposite.
    #[test]
    fn default_sandbox_args_uses_the_verified_dont_ask_mode() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(&Default::default(), &Default::default());
        assert_eq!(
            &args[0..2],
            &["--permission-mode".to_string(), "dontAsk".to_string()]
        );
    }

    /// Fix round 2 (2026-08-22): `dontAsk` alone denies every unapproved
    /// action, including a legitimate in-repo write -- inert, not "runs
    /// freely inside the workspace". `SHIPPED_POSTURE_ALLOW`/`_DENY` is
    /// what makes it usable; this pins the generated argv shape and that
    /// every entry from the shared list actually landed, so the two lists
    /// (source of truth and generated argv) can never silently drift.
    #[test]
    fn default_sandbox_args_generates_the_allow_and_deny_lists_from_the_shared_source() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(&Default::default(), &Default::default());
        assert_eq!(args.len(), 4, "got {args:?}");
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("a --disallowedTools= token");
        for (rule, _) in super::super::SHIPPED_POSTURE_ALLOW {
            assert!(
                allow_arg.contains(rule),
                "allow rule '{rule}' missing from {allow_arg}"
            );
        }
        for (rule, _) in super::super::SHIPPED_POSTURE_DENY {
            assert!(
                deny_arg.contains(rule),
                "deny rule '{rule}' missing from {deny_arg}"
            );
        }
        // Both single `=`-bound tokens, the same discipline `read_only_args`
        // already holds `--disallowedTools` to (the "I6 fix round": a
        // two-token form was verified to swallow the next argv entry).
        assert!(allow_arg.starts_with("--allowedTools="));
        assert!(deny_arg.starts_with("--disallowedTools="));
    }

    /// Issue #83: `default_sandbox_args` now projects `safety::SafetyPolicy`
    /// (derived from `SHIPPED_POSTURE_ALLOW`/`_DENY`) instead of iterating
    /// those constants directly. Under the shipped default -- no
    /// `[safety]`/`sandbox.extra_*` configured, i.e. exactly the two
    /// `Default::default()` values every other test in this file already
    /// passes -- the generated argv must stay byte-for-byte identical to
    /// what a hand-built projection straight from `SHIPPED_POSTURE_ALLOW`/
    /// `_DENY` would produce, so this refactor could not have silently
    /// changed a live-verified permission set.
    #[test]
    fn default_sandbox_args_stays_byte_identical_to_the_pre_safety_shipped_default() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(&Default::default(), &Default::default());

        // `SHIPPED_POSTURE_ALLOW`'s own first two entries already are
        // `Read(./**)`/`Edit(./**)` -- the projection prepends them
        // separately only because `safety::builtin_allow` filters them out
        // (they are not `Bash(...)`-wrapped commands), so iterating the
        // original constant directly reproduces the exact same order with
        // no duplication.
        let expected_allow: Vec<String> = super::super::SHIPPED_POSTURE_ALLOW
            .iter()
            .map(|(rule, _)| rule.to_string())
            .collect();
        let expected_deny: Vec<String> = super::super::SHIPPED_POSTURE_DENY
            .iter()
            .map(|(rule, _)| rule.to_string())
            .collect();

        assert_eq!(
            args,
            vec![
                "--permission-mode".to_string(),
                "dontAsk".to_string(),
                format!("--allowedTools={}", expected_allow.join(",")),
                format!("--disallowedTools={}", expected_deny.join(",")),
            ]
        );
    }

    /// Fix round 3 (2026-08-22): an operator's own `sandbox.extra_allow`/
    /// `extra_deny` (`SandboxConfig`, `config.rs`) are appended after the
    /// shipped lists, never replacing them -- the shipped entries must
    /// still be present alongside the operator's own addition.
    #[test]
    fn default_sandbox_args_appends_the_operators_own_extra_allow_and_deny() {
        let adapter = ClaudeAdapter::new(None);
        let sandbox = crate::commands::ctx::config::SandboxConfig {
            enabled: true,
            extra_allow: vec!["Bash(just test *)".to_string()],
            extra_deny: vec!["Bash(terraform apply *)".to_string()],
        };
        let args = adapter.default_sandbox_args(&sandbox, &Default::default());
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("a --disallowedTools= token");
        assert!(allow_arg.contains("Bash(just test *)"), "got {allow_arg}");
        assert!(
            allow_arg.contains("Read(./**)"),
            "the shipped entries must still be present, not replaced: {allow_arg}"
        );
        assert!(
            deny_arg.contains("Bash(terraform apply *)"),
            "got {deny_arg}"
        );
        assert!(
            deny_arg.contains("Bash(rm -rf *)"),
            "the shipped deny entries must still be present, not replaced: {deny_arg}"
        );
    }

    /// The scoping rule verified live to actually confine a write to the
    /// workspace is `Edit(./**)`, not a bare `Write` -- see `SHIPPED_
    /// POSTURE_ALLOW`'s own doc comment for the exact CLI error that
    /// disqualified `Write(./**)`. A bare, unscoped `Write`/`Edit` must
    /// never appear: it was verified live to let a write reach the
    /// directory *above* the workspace with no denial at all.
    #[test]
    fn default_sandbox_args_scopes_file_edits_to_the_workspace_not_bare_write() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(&Default::default(), &Default::default());
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("an --allowedTools= token");
        assert!(allow_arg.contains("Edit(./**)"), "got {allow_arg}");
        assert!(
            !allow_arg.contains("Write(") && !allow_arg.split(',').any(|t| t == "Write"),
            "a bare/unscoped Write rule was verified live to leak outside the workspace: \
             {allow_arg}"
        );
    }

    /// Must never be the dangerous bypass, under any circumstance.
    #[test]
    fn default_sandbox_args_never_emits_the_dangerous_bypass_flag() {
        let adapter = ClaudeAdapter::new(None);
        let args = adapter.default_sandbox_args(&Default::default(), &Default::default());
        assert!(
            !args
                .iter()
                .any(|a| a.contains("dangerously-skip-permissions")
                    || a.contains("bypassPermissions")),
            "must never widen: {args:?}"
        );
    }

    /// A multi-word agent bin (`ZIRV_CTX_AGENT_BIN="sh /tmp/stub.sh"`) has to work
    /// for all three invocation kinds: exec restarts build headless commands,
    /// handoff distillation builds a distiller command, and wrap restarts build an
    /// interactive one.
    #[test]
    fn a_multi_word_agent_bin_is_split_across_every_command_kind() {
        let adapter = ClaudeAdapter::new(Some("sh /tmp/stub.sh"));

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
                "-p".to_string(),
                "go".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
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

        let distiller = adapter.distiller_cmd("haiku");
        assert_eq!(distiller.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = distiller
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "-p".to_string(),
                "--model".to_string(),
                "haiku".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
                "--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string(),
            ]
        );
    }

    #[test]
    fn a_single_word_bin_and_extra_whitespace_still_work() {
        let adapter = ClaudeAdapter::new(Some("  /opt/homebrew/bin/claude  "));
        let cmd = adapter.interactive_cmd(None, &[]);
        assert_eq!(
            cmd.get_program().to_string_lossy(),
            "/opt/homebrew/bin/claude"
        );
        assert_eq!(cmd.get_args().count(), 0);
    }

    #[test]
    fn turn_signal_setup_exports_socket_and_session() {
        let adapter = ClaudeAdapter::new(None);
        let session = SessionRef {
            id: SessionId::parse("sess-1"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        let setup = adapter.register_turn_signal(&session, std::path::Path::new("/tmp/s/ab.sock"));
        assert!(
            setup
                .env
                .contains(&(SOCKET_ENV.to_string(), "/tmp/s/ab.sock".to_string()))
        );
        assert!(
            setup
                .env
                .contains(&(SESSION_ENV.to_string(), "sess-1".to_string()))
        );
        assert!(
            setup.instructions.contains("zirv ctx hook stop"),
            "instructions should name the hook command: {}",
            setup.instructions
        );
    }

    #[test]
    fn structural_context_extracts_prompts_files_and_errors() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"first prompt"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/lib.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"boom: file missing","is_error":true}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"[zirv] fixed it"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            "\n"
        );
        let ctx = structural_context(jsonl, 5);
        assert_eq!(ctx.user_messages, vec!["first prompt", "second prompt"]);
        assert_eq!(ctx.assistant_texts, vec!["[zirv] fixed it"]);
        assert_eq!(ctx.files_touched, vec!["/work/src/lib.rs"]);
        assert_eq!(ctx.tool_errors.len(), 1);
        assert!(ctx.tool_errors[0].contains("boom"));
    }

    #[test]
    fn structural_context_keeps_only_the_last_n_and_dedupes_files() {
        let mut jsonl = String::new();
        for i in 0..6 {
            jsonl.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"p{i}\"}}}}\n"
            ));
            jsonl.push_str(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{\"file_path\":\"/same.rs\"}}],\"usage\":{}}}\n",
            );
        }
        let ctx = structural_context(&jsonl, 2);
        assert_eq!(ctx.user_messages, vec!["p4", "p5"]);
        assert_eq!(ctx.files_touched, vec!["/same.rs"]);
    }

    #[test]
    fn the_system_prompt_becomes_the_verified_flag_pair() {
        // Exactly the mechanism recorded in
        // docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md.
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.system_prompt_args("be consistent"),
            vec![
                "--append-system-prompt".to_string(),
                "be consistent".to_string()
            ]
        );
    }

    #[test]
    fn an_empty_prompt_injects_nothing() {
        let adapter = ClaudeAdapter::new(None);
        assert!(adapter.system_prompt_args("").is_empty());
        assert!(adapter.system_prompt_args("   \n").is_empty());
    }

    /// I2: this must name the exact flag `system_prompt_args` emits, so a
    /// caller can find a user's own use of it and merge rather than override.
    #[test]
    fn the_user_facing_flag_name_matches_what_system_prompt_args_emits() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.user_system_prompt_flag(),
            Some("--append-system-prompt")
        );
    }

    #[test]
    fn claude_advertises_the_capability() {
        assert!(ClaudeAdapter::new(None).capabilities().system_prompt);
    }

    #[test]
    fn model_args_uses_the_verified_flag() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.model_args("opus"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    /// C: claude keeps a real default so `resolve_distiller_model` never has
    /// to fall back to an empty model for it, unlike codex.
    #[test]
    fn claude_defaults_the_distiller_model_to_haiku() {
        assert_eq!(
            ClaudeAdapter::new(None).default_distiller_model(),
            Some("haiku")
        );
    }

    /// A delegated headless worker with no operator `worker.claude` override
    /// gets claude's own hard default, not the operator's interactive seat
    /// model -- see `adapters::resolve_worker_model`.
    #[test]
    fn claude_defaults_the_worker_model_to_sonnet() {
        assert_eq!(
            ClaudeAdapter::new(None).default_worker_model(),
            Some("sonnet")
        );
    }

    /// Claude's two role layers are distinct texts, and the worker one carries
    /// none of the orchestrator's own delegate-everything coaching: a session
    /// that was itself delegated to must not be told its job is to delegate.
    #[test]
    fn claude_has_its_own_worker_layer_distinct_from_the_orchestrator_layer() {
        let layer = ClaudeAdapter::new(None)
            .worker_system_prompt()
            .expect("claude has a worker layer");
        assert_eq!(layer, WORKER_PROMPT);
        assert!(layer.starts_with("zirv worker conventions"));
        assert!(
            !layer.contains("Coordination and judgment are the job"),
            "the worker layer must not carry the orchestrator's own coaching: {layer}"
        );
        for claim in ["never run `zirv agent`", "fork-type subagents"] {
            assert!(
                layer.contains(claim),
                "the worker layer must say '{claim}': {layer}"
            );
        }
    }

    /// The claude ladder, top to bottom: fable/mythos, opus, sonnet, haiku.
    /// `review_model_below` returns the tier one below `seat`; an unknown or
    /// absent seat assumes the top tier, and haiku (already the floor) maps
    /// to itself rather than falling off the ladder.
    #[test]
    fn review_model_below_walks_the_claude_ladder() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(adapter.review_model_below(Some("claude-fable-5")), "opus");
        assert_eq!(adapter.review_model_below(Some("mythos")), "opus");
        assert_eq!(adapter.review_model_below(Some("opus")), "sonnet");
        assert_eq!(adapter.review_model_below(Some("sonnet")), "haiku");
        assert_eq!(adapter.review_model_below(Some("haiku")), "haiku");
        assert_eq!(
            adapter.review_model_below(None),
            "opus",
            "no seat configured: assume the top tier"
        );
        assert_eq!(
            adapter.review_model_below(Some("some-unreleased-model")),
            "opus",
            "unrecognised seat: assume the top tier"
        );
    }

    /// Seat matching must be case-insensitive: a mixed-case seat like
    /// "Opus" (or a full id with mixed-case segments) must land on the same
    /// ladder rung as its lowercase form, not fall through to the unknown
    /// arm and assume the top tier.
    #[test]
    fn review_model_below_matches_the_seat_case_insensitively() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(adapter.review_model_below(Some("Opus")), "sonnet");
        assert_eq!(
            adapter.review_model_below(Some("claude-Opus-4-5")),
            "sonnet"
        );
        assert_eq!(adapter.review_model_below(Some("SONNET")), "haiku");
        assert_eq!(adapter.review_model_below(Some("Haiku")), "haiku");
        assert_eq!(adapter.review_model_below(Some("Fable")), "opus");
        assert_eq!(adapter.review_model_below(Some("MYTHOS")), "opus");
    }

    #[test]
    fn resume_args_uses_the_verified_flag() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.resume_args("sess-1"),
            Some(vec!["--resume".to_string(), "sess-1".to_string()])
        );
    }

    #[test]
    fn the_file_flag_name_is_the_documented_one() {
        assert_eq!(
            ClaudeAdapter::new(None).system_prompt_file_flag(),
            Some("--append-system-prompt-file")
        );
    }

    /// Writes a throwaway `--help` stub so the probe can be exercised without
    /// depending on the machine's actual installed binary. The heredoc keeps
    /// `help_text` free of shell-escaping concerns. Every caller spawns it via
    /// `sh`, so it is unix-only like they are.
    #[cfg(unix)]
    fn help_stub(dir: &std::path::Path, name: &str, help_text: &str) -> String {
        let script = dir.join(name);
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat <<'EOF'\n{help_text}\nEOF\n"),
        )
        .expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        format!("sh {}", script.display())
    }

    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_detects_the_flag_from_help_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(
            dir.path(),
            "probe-yes.sh",
            "Options:\n  --append-system-prompt-file <path>",
        );
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(adapter.supports_system_prompt_file(&[]));
    }

    /// Verified against the real CLI (`claude --help`, v2.1.220): the flag is
    /// never spelled out on its own; it only appears folded into this exact
    /// shorthand, inside the `--bare` option's own description. A probe that
    /// only looked for the plain flag text would report "unsupported" on the
    /// very machine this was verified on.
    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_detects_the_real_clis_bracket_shorthand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(
            dir.path(),
            "probe-bracket.sh",
            "Explicitly provide context via: --system-prompt[-file],\n  \
             --append-system-prompt[-file], --add-dir (CLAUDE.md dirs)",
        );
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(
            adapter.supports_system_prompt_file(&[]),
            "must recognize the bracket-shorthand form the real CLI actually uses"
        );
    }

    #[test]
    fn normalizes_to_advertise_the_file_flag_matches_both_spellings() {
        assert!(normalizes_to_advertise_the_file_flag(
            "--append-system-prompt[-file] <path>"
        ));
        assert!(normalizes_to_advertise_the_file_flag(
            "--append-system-prompt-file <path>"
        ));
        assert!(!normalizes_to_advertise_the_file_flag(
            "--append-system-prompt <prompt>"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_is_false_when_help_omits_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(dir.path(), "probe-no.sh", "nothing relevant here");
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(!adapter.supports_system_prompt_file(&[]));
    }

    #[test]
    fn supports_system_prompt_file_fails_open_when_the_binary_is_missing() {
        let adapter = ClaudeAdapter::new(Some("/nonexistent/definitely-not-a-binary"));
        assert!(
            !adapter.supports_system_prompt_file(&[]),
            "a probe failure must never block a launch"
        );
    }

    /// M7: "probe... once per launch (cache the result in-process across
    /// restarts)". Rewriting the stub after the first probe must not change
    /// the cached answer: a restart inside the same run must never re-spawn
    /// `--help`.
    #[cfg(unix)]
    #[test]
    fn supports_system_prompt_file_is_cached_after_the_first_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = help_stub(dir.path(), "probe-cache.sh", "--append-system-prompt-file");
        let adapter = ClaudeAdapter::new(Some(&bin));
        assert!(adapter.supports_system_prompt_file(&[]));

        let script = dir.path().join("probe-cache.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'nothing now\\n'\n").expect("rewrite stub");
        assert!(
            adapter.supports_system_prompt_file(&[]),
            "the first probe's answer is cached for the life of the process"
        );
    }

    /// Regression: the cache used to be keyed by joining `program` and
    /// `bin_args` into one string, which made two distinct commands collide
    /// on the same key (e.g. `("sh /a", ["--help"])` and `("sh", ["/a",
    /// "--help"])` both joined to `"sh /a --help"`). Same `program` ("sh"),
    /// different `bin_args` (two different scripts, one supporting the flag
    /// and one not) must be cached independently, not share one answer.
    #[cfg(unix)]
    #[test]
    fn the_cache_key_distinguishes_different_bin_args_for_the_same_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supports = dir.path().join("supports.sh");
        std::fs::write(
            &supports,
            "#!/bin/sh\ncat <<'EOF'\n--append-system-prompt-file\nEOF\n",
        )
        .expect("write");
        let unsupported = dir.path().join("unsupported.sh");
        std::fs::write(&unsupported, "#!/bin/sh\ncat <<'EOF'\nnothing here\nEOF\n").expect("write");
        for script in [&supports, &unsupported] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        assert!(
            probe_system_prompt_file_support("sh", &[supports.display().to_string()]),
            "the supporting script's own answer must not be shadowed by the other's"
        );
        assert!(
            !probe_system_prompt_file_support("sh", &[unsupported.display().to_string()]),
            "the non-supporting script must get its own answer, not the cached true from above"
        );
    }

    #[test]
    fn the_prompt_args_compose_with_the_existing_command_builders() {
        let adapter = ClaudeAdapter::new(None);
        let mut extra = adapter.system_prompt_args("be consistent");
        extra.push("--model".to_string());
        extra.push("sonnet".to_string());

        let headless = adapter.headless_cmd("go", &SessionId::parse("abc"), &extra);
        assert_eq!(
            built_args(&adapter, &headless),
            vec![
                "-p".to_string(),
                "go".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--append-system-prompt".to_string(),
                "be consistent".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ]
        );

        let interactive = adapter.interactive_cmd(None, &extra);
        assert_eq!(
            built_args(&adapter, &interactive)[0],
            "--append-system-prompt"
        );
    }

    #[test]
    fn structural_context_survives_the_real_fixture() {
        let jsonl =
            std::fs::read_to_string(fixture_path("claude-real-session.jsonl")).expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");
        let recorded = expected["files_touched_min"].as_u64().unwrap_or(0);
        assert!(
            structural_context(&jsonl, 1_000).files_touched.len() as u64 >= recorded,
            "files_touched should find at least the recorded count"
        );

        let ctx = structural_context(&jsonl, 5);
        assert!(ctx.user_messages.len() <= 5);
        assert!(
            ctx.files_touched.len() <= 5,
            "and then keep only the tail, like every other field: {}",
            ctx.files_touched.len()
        );
    }

    /// A handoff leaves as a single argv token, and Windows caps a command
    /// line at 32,767 characters. `files_touched` accumulated every unique
    /// path of the whole session while its neighbours were capped, so a long
    /// enough session could no longer relaunch at all.
    #[test]
    fn structural_context_caps_files_touched_like_every_other_field() {
        let mut jsonl = String::new();
        for index in 0..40 {
            jsonl.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{{\"file_path\":\"/src/file-{index}.rs\"}}}}],\"usage\":{{}}}}}}\n"
            ));
        }

        let ctx = structural_context(&jsonl, 5);
        assert_eq!(
            ctx.files_touched,
            vec![
                "/src/file-35.rs",
                "/src/file-36.rs",
                "/src/file-37.rs",
                "/src/file-38.rs",
                "/src/file-39.rs"
            ],
            "the newest paths are the ones a handoff is worth carrying"
        );
    }

    #[test]
    fn claude_contributes_the_orchestrator_layer_and_it_names_claudes_own_tools() {
        let layer = ClaudeAdapter::new(None)
            .base_system_prompt()
            .expect("claude has a base layer of its own");
        assert_eq!(layer, ORCHESTRATOR_PROMPT);
        for claude_specific in ["Agent tool", ".claude/agents", "/code-review"] {
            assert!(
                layer.contains(claude_specific),
                "the layer is claude-specific by construction: '{claude_specific}'"
            );
        }
    }

    /// The review bullet must route review to the harness roster's own
    /// configured review model rather than let it silently run on this
    /// seat's own model, must cap fan-out at a single-reviewer effort level,
    /// and the model-routing bullet's "pin" clause must carve out that one
    /// exception rather than blanket-forbid every override.
    #[test]
    fn the_orchestrator_prompt_routes_review_to_the_rosters_configured_model() {
        assert!(
            ORCHESTRATOR_PROMPT.contains(
                "routed to the review model named in the harness roster, never this seat's own \
                 model"
            ),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("single-reviewer effort level (low or medium)"),
            "never a high-or-above fan-out from this seat: {ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains(
                "do not override those, except the review-model rule below, which outranks this"
            ),
            "the model-routing bullet's pin clause must carve out the review-model exception: \
             {ORCHESTRATOR_PROMPT}"
        );
    }

    /// TASK 1: every Agent-tool dispatch must set the model explicitly, the
    /// seat's own model and fork-type subagents are both off limits, and
    /// token economy applies to both this seat's own replies and every
    /// subagent brief it writes.
    #[test]
    fn the_orchestrator_prompt_encodes_model_routing_and_token_economy() {
        assert!(
            ORCHESTRATOR_PROMPT.contains("MUST set the model parameter explicitly"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        for tier in [
            "haiku for mechanical",
            "sonnet for ordinary",
            "opus only for hard",
        ] {
            assert!(
                ORCHESTRATOR_PROMPT.contains(tier),
                "missing tier guidance '{tier}': {ORCHESTRATOR_PROMPT}"
            );
        }
        assert!(
            ORCHESTRATOR_PROMPT.contains("never dispatch on this seat's model"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("never use fork-type subagents from this seat"),
            "forks always inherit the seat model and ignore overrides: {ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("keep your own replies brief and decision-focused"),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains(
                "tell the worker to reply briefly with compact \
             structured findings, never raw file dumps"
            ),
            "got:\n{ORCHESTRATOR_PROMPT}"
        );
    }

    /// `exec` strips this many leading tokens off the argv the operator
    /// wrote before carrying the rest into a restart, so a rewrite that
    /// happens inside `base()` must not be counted here: the argv it applies
    /// to is the one the adapter builds, not the one it was handed.
    #[test]
    fn the_launch_prefix_length_counts_the_operators_argv_not_the_rewritten_one() {
        assert_eq!(ClaudeAdapter::new(None).launch_prefix_len(), 1);
        assert_eq!(
            ClaudeAdapter::new(Some("claude.cmd")).launch_prefix_len(),
            1,
            "a .cmd shim is still one program token in the operator's argv"
        );
        assert_eq!(
            ClaudeAdapter::new(Some("sh /tmp/stub.sh")).launch_prefix_len(),
            2
        );
    }

    /// An npm-installed `claude` on Windows is `claude.cmd`, which
    /// `CreateProcessW` rejects with `ERROR_BAD_EXE_FORMAT` (193). The
    /// adapter has to hand it to `cmd.exe` instead, and the tokens it adds
    /// have to lead the ones it was already going to pass.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_is_launched_through_cmd_exe_with_its_arguments_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = ClaudeAdapter::new(Some(&shim.display().to_string()));
        let cmd = adapter.interactive_cmd(Some("resume this"), &["--continue".to_string()]);

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
                "--continue".to_string(),
            ]
        );
        assert_eq!(
            adapter.launch_prefix_len(),
            1,
            "and the rewrite never changes what exec strips off the operator's argv"
        );
    }

    /// The rewrite is a Windows concern only: everywhere else the program is
    /// spawned exactly as written, shebang and all.
    #[cfg(not(windows))]
    #[test]
    fn a_program_is_spawned_exactly_as_written_off_windows() {
        let adapter = ClaudeAdapter::new(Some("/opt/claude.cmd"));
        let cmd = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(cmd.get_program().to_string_lossy(), "/opt/claude.cmd");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["resume this".to_string()]);
    }
}
