//! Supervises one headless run, restarting it on rot with a distilled
//! handoff. Mail (`super::mail`) is delivered into the composed system
//! prompt exactly once, at the very first launch computed in `run_with`: a
//! rot/timeout restart or a usage-limit park reuses that same launch's
//! `prompt_args` (the argv already carrying the composed text, whichever
//! mechanism delivered it), it does not recompute the composed prompt or
//! re-list mail. A message that arrives after the run has started is
//! therefore not retroactively injected into it -- the next `zirv ctx exec`
//! invocation (or a `zirv ctx loop` cycle, which re-lists mail every cycle
//! by design) picks it up instead.
//!
//! N4's `zirv ctx nudge` is the one deliberate exception: a nudge relaunch
//! recomposes the prompt and re-lists mail (scoped to the session that was
//! nudged) precisely because that recompute is the whole point -- see the
//! `nudged` branch in the main loop below.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::pace;
use super::rot::Verdict;
use super::signal::{self, TurnSignal};
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick};
use super::{CtxResult, adapters, handoff, log, score};

/// The restart budget is spent and the session is still rotting. Callers apply
/// their own policy from here.
pub const EXIT_ROT_EXHAUSTED: i32 = 75;
/// Wall-clock timeout with no restarts left.
pub const EXIT_TIMEOUT: i32 = 76;

/// The supervisor reports its own outcomes through the same `i32` an agent's
/// exit code arrives on, so "exited with code 75" reads as something the
/// agent did rather than as zirv giving up. Shared by `zirv ctx agent`
/// (agent.rs) and script `agent:` steps (agent_command.rs), which both
/// delegate to this supervisor and want the same wording for the same two
/// outcomes.
pub fn describe_exit(code: i32) -> String {
    match code {
        EXIT_ROT_EXHAUSTED => "the session kept rotting and the restart budget ran out".to_string(),
        EXIT_TIMEOUT => "the supervised run hit its wall-clock timeout".to_string(),
        other => format!("exited with code {other}"),
    }
}

#[derive(Debug, clap::Args)]
pub struct ExecArgs {
    /// Adapter name: claude or codex. Detected from the command when omitted.
    #[arg(long)]
    pub agent: Option<String>,
    /// Session id of the supervised run, used to locate its transcript.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Transcript path, when the agent writes somewhere the adapter cannot derive.
    #[arg(long)]
    pub transcript: Option<PathBuf>,
    /// Prompt to reuse on restart. Extracted from the command when omitted.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Restart budget before giving up.
    #[arg(long)]
    pub max_restarts: Option<u32>,
    /// Wall-clock limit for the whole supervised run.
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    /// The headless agent command, after `--`.
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
}

/// Flags that pin a launch to a conversation that already exists. A restart is
/// a deliberate escape from the session that rotted, so inheriting any of them
/// would march the fresh child straight back into it and burn the whole
/// restart budget re-entering rot. The first two carry a value; the rest are
/// bare.
const RESUME_FLAGS_WITH_VALUE: [&str; 2] = ["--session-id", "--resume"];
const RESUME_FLAGS_BARE: [&str; 3] = ["-c", "--continue", "--fork-session"];

/// True for `--resume=abc` when `--resume` is in `flags`: the CLIs accept both
/// spellings, so stripping only the two-token form leaves the other behind.
fn is_joined_form(arg: &str, flags: &[&str]) -> bool {
    arg.split_once('=')
        .is_some_and(|(name, _)| flags.contains(&name))
}

/// Locates the token that carries the prompt in a headless agent command, and
/// the prompt itself when that token is followed by one.
///
/// `known` is the prompt zirv already holds for this run (`--prompt`, or an
/// agent step's own text). Given it, the value is recognised by equality
/// instead of by shape, which is the only way to tell a prompt that happens to
/// begin with `-` -- a markdown bullet list, say -- from a genuine second
/// flag. Without it the shape heuristic still applies, because guessing wrong
/// about a restart's prompt is worse than not restarting.
///
/// The value is `None` for a bare flag: `-p` with another flag after it (or
/// nothing at all) means the prompt arrives on stdin. The flag itself still
/// has to be stripped from a restart argv, but the token after it must not be.
fn locate_prompt(
    command: &[String],
    prefix: usize,
    known: Option<&str>,
) -> Option<(usize, Option<String>)> {
    for (index, arg) in command.iter().enumerate().skip(prefix) {
        let is_prompt_flag = arg == "-p" || arg == "--print";
        let is_subcommand = arg == "exec";
        if !is_prompt_flag && !is_subcommand {
            continue;
        }
        let Some(next) = command.get(index + 1) else {
            return Some((index, None));
        };
        if Some(next.as_str()) == known {
            return Some((index, Some(next.clone())));
        }
        if next.starts_with('-') {
            return Some((index, None));
        }
        return Some((index, Some(next.clone())));
    }
    None
}

/// Finds the prompt in a headless agent command. Returns `None` rather than
/// guessing: a restart with the wrong prompt is worse than no restart.
pub fn extract_prompt(command: &[String]) -> Option<String> {
    locate_prompt(command, 1, None).and_then(|(_, prompt)| prompt)
}

/// M8: the user's own flags from the original `--` command, with only what
/// zirv itself re-supplies on every restart removed. Everything else the
/// operator passed -- `--model`, `--allowedTools`, anything at all -- must
/// reach a restarted child exactly as it reached the first one; silently
/// dropping it here was the asymmetry M8 fixed (zirv's own added flags, e.g.
/// the system prompt, always survived a restart; the operator's own did not).
///
/// Three kinds of token are dropped. The prompt, because every relaunch
/// regenerates it to carry a handoff. Anything pinning the launch to an
/// existing conversation, because the relaunch is escaping one. And the
/// leading tokens of the program invocation itself -- `prefix` of them from
/// the adapter, plus any further positional before the first flag, which is
/// how `npx claude ...` and a positional prompt both look -- because
/// `headless_cmd` rebuilds the invocation and re-appending them would leave a
/// stray argument the agent reads as a second prompt.
pub fn extra_launch_flags(
    command: &[String],
    prefix: usize,
    known_prompt: Option<&str>,
) -> Vec<String> {
    let located = locate_prompt(command, prefix, known_prompt);
    let prompt_at = located.as_ref().map(|(index, _)| *index);
    let prompt_takes_value = located.is_some_and(|(_, value)| value.is_some());

    let mut out = Vec::with_capacity(command.len());
    let mut skip_next = false;
    let mut in_prefix = true;
    for (index, arg) in command.iter().enumerate().skip(prefix) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if Some(index) == prompt_at {
            skip_next = prompt_takes_value;
            in_prefix = false;
            continue;
        }
        if in_prefix && !arg.starts_with('-') {
            continue;
        }
        in_prefix = false;

        if is_joined_form(arg, &RESUME_FLAGS_WITH_VALUE) || is_joined_form(arg, &RESUME_FLAGS_BARE)
        {
            continue;
        }
        if RESUME_FLAGS_WITH_VALUE.contains(&arg.as_str()) {
            // A bare `--resume` with a flag after it takes no value, so the
            // next token belongs to the operator and has to survive.
            skip_next = command
                .get(index + 1)
                .is_some_and(|next| !next.starts_with('-'));
            continue;
        }
        if RESUME_FLAGS_BARE.contains(&arg.as_str()) {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// Compaction of a headless run is pointless (there is no TUI to type into), so
/// only a restart verdict acts, and only for the session this supervisor owns:
/// the socket is named after eight hex characters of a session id, so a stale
/// hook can reach it and must not be able to kill a healthy child.
pub fn should_stop_for_signal(signal: &TurnSignal, session: &str) -> bool {
    signal.session_id == session && signal.verdict == Verdict::Restart
}

fn build_command(command: &[String], repo: &Path) -> CtxResult<Command> {
    let (program, rest) = command
        .split_first()
        .ok_or("no command to supervise; pass it after --")?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(repo);
    Ok(cmd)
}

pub fn run_with<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    // Gated only by `cfg.chrome.events` (which already folds in `--quiet` on
    // `zirv ctx agent`, `ZIRV_CTX_QUIET` and `[chrome] events`), independent
    // of whatever terminal (if any) is attached: a headless supervised run
    // still wants these lines on its stderr.
    let announcer =
        super::announce::Announcer::new(cfg.chrome.events, console::colors_enabled_stderr());
    let agent_name = args.agent.as_deref().or(cfg.agent.as_deref());
    let adapter = adapters::select(agent_name, &args.command, &cfg)?;
    let state = StateDir::resolve(env)?;
    // Computed early (`mail_slug` further down reuses this exact value)
    // because both the memory layer below and the mail layer need a slug,
    // and the memory layer's own read has to happen before the first
    // `compose` call.
    let mail_slug = super::state::repo_slug(repo);
    // N5: this run's memory bank, already rendered (age included) so
    // `prompt::compose` itself never has to read a clock. Loaded once, here,
    // and reused verbatim by the nudge-restart recompose below -- unlike
    // mail, which is deliberately re-listed narrowly by session on that
    // path, the memory bank is repo-wide and does not go stale within one
    // `run_with` call the way a specific session's mailbox does.
    let memory_entries = super::memory::render_for_prompt(&state, &mail_slug, &cfg, now_secs());

    // A wrapped command that matches no adapter (no explicit `--agent`,
    // detection came up empty) is not actually the agent whose flags we would
    // be injecting; see the matching gate in wrap.rs.
    let skip_injection = args.simple
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    let composed = super::prompt::compose(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        skip_injection,
        &cfg.prompt,
        super::prompt::PromptRole::Worker,
        &memory_entries,
        cfg.memory.max_injected_bytes,
    );
    // Known before argv is touched, because it decides how argv is read: the
    // token holding this exact text is the prompt, whatever it looks like.
    let prompt = args
        .prompt
        .clone()
        .or_else(|| extract_prompt(&args.command));
    // An argv that names no program -- empty, or starting with a flag -- is
    // not a command to pass through: the adapter builds the launch and these
    // are extra flags for it. That is how an agent step arrives, holding its
    // prompt as data with no argv to encode it into.
    let adapter_builds_launch = args
        .command
        .first()
        .is_none_or(|first| first.starts_with('-'));
    let prefix = if adapter_builds_launch {
        0
    } else {
        adapter.launch_prefix_len()
    };
    // The prompt is data, not argv to be interpreted. Protecting its index
    // keeps a prompt that happens to read like the adapter's own
    // system-prompt flag from being stripped out of the launch and promoted
    // into the composed prompt as an operator instruction.
    let prompt_value_at = locate_prompt(&args.command, prefix, prompt.as_deref())
        .and_then(|(index, value)| value.map(|_| index + 1));

    // Determined before mail is listed (N3: delivery is scoped to this
    // session's own short id, so the id has to exist first) and before
    // `prompt_args` (M7 needs a session id to name the private prompt file
    // after) rather than after, as this used to be.
    let session_raw = args
        .session_id
        .clone()
        .unwrap_or_else(|| SessionId::new_v4().to_string());
    let mut session = SessionId::parse(&session_raw);

    // Mail is delivered once, here, at the first launch: every restart below
    // reuses this same `composed` value (see the module doc), so a message
    // that arrives mid-run is not retroactively injected into an
    // already-running session. `run_loop`, by contrast, starts a fresh
    // session every cycle and re-lists mail on each one.
    //
    // `mut`: drained by the loop below, once, right after the first
    // successful spawn -- not here. Consuming this early (Item 3's fix) used
    // to mark the mail read before any child had actually started: a launch
    // that fails to spawn at all, or a long pacing park ahead of it, moved
    // it to `read/` with no session ever having seen it.
    //
    // N3: scoped to this session's own short id, so a message addressed to a
    // different session (`send --to-session`) never leaks into this launch's
    // prompt just because the two share a repo and an agent name.
    let session_short = super::sessions::short_id(session.as_str());
    let mut mail_entries: Vec<(PathBuf, super::mail::Message)> =
        if composed.is_some() && cfg.mail.enabled {
            super::mail::list(
                &state,
                &mail_slug,
                Some(adapter.name()),
                Some(&session_short),
            )
            .unwrap_or_default()
        } else {
            Vec::new()
        };
    let mail_messages: Vec<super::mail::Message> =
        mail_entries.iter().map(|(_, msg)| msg.clone()).collect();
    if !mail_messages.is_empty() {
        announcer.emit(&super::announce::Event::MailDelivered {
            count: mail_messages.len(),
        });
    }
    let composed =
        super::prompt::with_mail_layer(composed, &mail_messages, cfg.mail.max_delivered_bytes);

    // The first spawn's own argv may already carry the adapter's system-prompt
    // flag (e.g. `-- claude --append-system-prompt "..."`); merge it in rather
    // than letting `prompt_args` silently override it below.
    // `mut`: a nudge relaunch (N4) recomposes fresh, mail included, and
    // replaces this binding so any restart or park after it keeps using the
    // nudge-enriched prompt rather than silently reverting to the launch-time
    // one.
    let (launch_command, mut composed) = super::prompt::merge_command_line_prompt(
        adapter.as_ref(),
        &args.command,
        composed,
        prompt_value_at,
    );

    // The user's own flags from the original `--` command (anything beyond
    // the prompt and the session-pinning flags, all of which every restart
    // regenerates fresh): see `extra_launch_flags`. M8: a restart used to
    // rebuild the command from scratch with only zirv's own added flags,
    // silently dropping these.
    let user_extra = extra_launch_flags(&launch_command, prefix, prompt.as_deref());

    // The probe has to hit the binary that will actually be spawned. When the
    // argv names no program the adapter builds the launch, so there is nothing
    // in `launch_command` to probe -- it is flags, and `--model --help` is not
    // a capability check.
    let probe_target: &[String] = if adapter_builds_launch {
        &[]
    } else {
        &launch_command
    };
    // `mut`: recomputed by a nudge relaunch alongside `composed` above.
    let mut prompt_args = super::prompt::injection_args_for_session(
        adapter.as_ref(),
        probe_target,
        composed.as_ref(),
        &state,
        session.as_str(),
    );
    super::prompt::log_injection(
        &state,
        "exec",
        session.as_str(),
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    );
    announcer.emit(&super::prompt::injection_event(
        composed.as_ref(),
        adapter.capabilities().system_prompt,
    ));

    let derive_transcript = |session: &SessionId| {
        adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        })
    };

    // `--transcript` describes the caller's own first child only. Every restart
    // is a new session launched by the adapter, so its transcript path has to be
    // derived again or the watcher would keep polling the dead child's file.
    let mut transcript = args
        .transcript
        .clone()
        .unwrap_or_else(|| derive_transcript(&session));

    // Surfaced once, upfront, rather than only when a restart is already
    // needed: an operator who never rots would otherwise never learn that
    // rotting is a dead end for this invocation until it actually happens.
    if prompt.is_none() {
        writeln!(
            w,
            "zirv ctx exec: no prompt could be found in the command; restarts and usage-limit \
             parking will be unavailable for this run. Pass --prompt to enable them."
        )?;
    }
    let max_restarts = args.max_restarts.unwrap_or(cfg.supervise.max_restarts);
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(cfg.supervise.max_cycle_secs));
    let poll = Duration::from_millis(cfg.supervise.poll_ms);

    let socket_path = state.socket_for(session.as_str());
    let server = match signal::SignalServer::bind(&socket_path) {
        Ok(server) => Some(server),
        Err(e) => {
            // Turn signals only accelerate detection; polling is the floor.
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "no-socket",
                    detail: &e.to_string(),
                },
            );
            None
        }
    };
    // Rebuilt for every session, because the hook inside a child reports the
    // session id this exports. Pinning the first one makes every restart's
    // signals look like they belong to a session that is already dead.
    //
    // `AGENT_ENV` is exported unconditionally, unlike the turn-signal env
    // above (which needs a bound socket): it names the same fact
    // `ctx.toml`'s own `agent` config key would, so a nested `zirv ctx ...`
    // call inside this session's own children defaults to this session's own
    // harness rather than re-resolving from scratch, whether or not turn
    // signals are available.
    let turn_env_for = |session: &SessionId| {
        let mut env: Vec<(String, String)> = server
            .as_ref()
            .map(|server| {
                adapter
                    .register_turn_signal(
                        &SessionRef {
                            id: session.clone(),
                            cwd: repo.to_path_buf(),
                        },
                        server.path(),
                    )
                    .env
            })
            .unwrap_or_default();
        env.push((adapters::AGENT_ENV.to_string(), adapter.name().to_string()));
        env
    };

    // With no argv to pass through, the first launch is built exactly the way
    // every relaunch builds one. That symmetry is the point: a caller holding
    // the prompt as data never encodes it into argv for this function to
    // decode again, so it can never be misread as a flag.
    let mut command = if adapter_builds_launch {
        let prompt_text = prompt.as_deref().ok_or(
            "no command to supervise; pass the agent command after --, \
             or --prompt to have zirv build the launch itself",
        )?;
        let extra: Vec<String> = user_extra
            .iter()
            .cloned()
            .chain(prompt_args.iter().cloned())
            .collect();
        let mut command = adapter.headless_cmd(prompt_text, &session, &extra);
        command.current_dir(repo);
        command
    } else {
        let mut command = build_command(&launch_command, repo)?;
        for arg in &prompt_args {
            command.arg(arg);
        }
        command
    };
    for (key, value) in turn_env_for(&session) {
        command.env(key, value);
    }
    let mut restarts = 0;
    // N4: consecutive `zirv ctx nudge`-driven restarts, capped by `cfg.
    // supervise.max_nudges` -- a separate budget from `restarts` above,
    // since a nudge is not rot and must never spend it. A relaunch (nudge or
    // otherwise) needs a known prompt to carry forward; without one a nudge
    // is claimed but ignored, the same as being over the cap.
    let mut nudge_restarts = 0u32;
    let can_restart = prompt.is_some();
    let now_fn = now_secs;
    let sleep_fn = |d: Duration| std::thread::sleep(d);

    // Best-effort registration: covers a hand-typed `zirv ctx exec` as well
    // as `zirv ctx agent` and a script `agent:` step, both of which delegate
    // to this same function. Refreshed (not re-registered) whenever a
    // restart or a usage-limit park mints a fresh session id below, and
    // released explicitly in every arm that leaves this loop -- the same
    // explicit-arm discipline `RawGuard` follows, since this binary's
    // release profile is `panic = "abort"` and `Drop` is not guaranteed.
    let mut session_guard = super::sessions::SessionGuard::register(
        &state,
        super::sessions::Record::new(
            session.as_str(),
            adapter.name(),
            repo,
            super::sessions::Verb::Exec,
        ),
    );

    loop {
        pace::wait_for_window(
            w,
            &state,
            &cfg.pace,
            "exec",
            session.as_str(),
            &now_fn,
            &sleep_fn,
            Some(&announcer),
        );

        let (mut child, tap) = supervise::spawn_tapped(command)?;
        // Item 3: the messages folded into the launch prompt are consumed
        // here, right after the spawn that actually carried them has
        // genuinely started -- not before pacing or the spawn itself, where
        // a park or a failed launch would have moved them to `read/` with no
        // session ever having seen them. Drains to empty on the first
        // successful spawn, so a later restart's own iteration through this
        // same loop finds nothing left to consume and is a no-op. A failed
        // consume must not fail the launch itself -- best effort, like the
        // rest of state-dir housekeeping -- since the mail has already
        // reached the prompt either way.
        for (path, _) in mail_entries.drain(..) {
            let _ = super::mail::consume(&state, &mail_slug, &path);
        }
        // Fresh scorer per iteration, over the current session's transcript.
        let mut scorer = score::IncrementalScorer::new(transcript.clone());
        let mut rotted = false;
        let mut limit_hit = false;
        let mut nudged = false;

        let outcome = supervise_run(
            &mut child,
            Instant::now() + timeout,
            poll,
            &mut scorer,
            adapter.as_ref(),
            &cfg.score,
            &state,
            server.as_ref(),
            session.as_str(),
            &mut rotted,
            &tap,
            &mut limit_hit,
            &mut nudged,
            nudge_restarts,
            cfg.supervise.max_nudges,
            can_restart,
        )?;

        // `supervise_child` checks the child's exit status before calling the
        // tick, so a fast limit-hit exit (print the notice, exit immediately,
        // exactly what a real exhausted-window run looks like) can race past
        // the last tick that would have caught it. A final drain here closes
        // that race without touching supervise_child's general contract.
        if !limit_hit {
            limit_hit = pace::scan_for_limit(
                &tap.try_lines(),
                &state,
                session.as_str(),
                "exec",
                &mut std::io::stderr(),
            );
        }

        match outcome {
            Outcome::Exited(code) if !limit_hit => {
                session_guard.release();
                return Ok(code);
            }
            Outcome::Exited(_) | Outcome::TimedOut | Outcome::StoppedByTick(_) => {}
        }

        // N4: a nudge relaunch is neither a limit park nor a rot restart --
        // `supervise_run`'s own tick only ever sets `nudged` when a relaunch
        // is actually possible (a known prompt) and under the consecutive
        // cap, so this arm always follows through rather than needing its
        // own "no prompt"/"over budget" fallbacks the way rot's restart does.
        if nudged {
            let nudged_short = super::sessions::short_id(session.as_str());
            let prompt_text = prompt
                .clone()
                .expect("supervise_run only sets `nudged` when a prompt is known");

            let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
            let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
            let (note, source) = handoff::distill_or_structural(
                adapter.as_ref(),
                &cfg.handoff.model,
                &ctx,
                Duration::from_secs(cfg.handoff.timeout_secs),
            );
            let stored = handoff::store(&state, repo, session.as_str(), &note)?;

            session = SessionId::new_v4();
            session_guard.refresh_session(session.as_str());
            transcript = derive_transcript(&session);

            // Recompose fresh -- unlike an ordinary restart, which reuses
            // the launch-time `composed`/`prompt_args` untouched (see this
            // module's own doc comment), a nudge relaunch is explicitly the
            // chance to pick up what prompted it: the nudge's own payload
            // arrived as ordinary session-addressed mail (`sessions::run_
            // nudge_with` stores it before writing the wake-up marker), so
            // re-listing mail for the session that was just nudged and
            // folding it in through `with_mail_layer` delivers it with zero
            // new injection machinery.
            let mut fresh = super::prompt::compose(
                crate::utils::home_dir().ok().as_deref(),
                repo,
                skip_injection,
                &cfg.prompt,
                super::prompt::PromptRole::Worker,
                &memory_entries,
                cfg.memory.max_injected_bytes,
            );
            let nudge_mail: Vec<(PathBuf, super::mail::Message)> = if cfg.mail.enabled {
                super::mail::list(
                    &state,
                    &mail_slug,
                    Some(adapter.name()),
                    Some(&nudged_short),
                )
                .unwrap_or_default()
            } else {
                Vec::new()
            };
            let nudge_mail_msgs: Vec<super::mail::Message> =
                nudge_mail.iter().map(|(_, msg)| msg.clone()).collect();
            if !nudge_mail_msgs.is_empty() {
                announcer.emit(&super::announce::Event::MailDelivered {
                    count: nudge_mail_msgs.len(),
                });
            }
            fresh = super::prompt::with_mail_layer(
                fresh,
                &nudge_mail_msgs,
                cfg.mail.max_delivered_bytes,
            );
            let (_, fresh) = super::prompt::merge_command_line_prompt(
                adapter.as_ref(),
                &launch_command,
                fresh,
                prompt_value_at,
            );
            composed = fresh;
            prompt_args = super::prompt::injection_args_for_session(
                adapter.as_ref(),
                probe_target,
                composed.as_ref(),
                &state,
                session.as_str(),
            );
            // Folded into the prompt above, but only actually marked read
            // once the relaunch that carries it genuinely spawns -- the same
            // Item 3 discipline every other delivery seam in this function
            // follows.
            mail_entries = nudge_mail;

            super::prompt::log_injection(
                &state,
                "exec",
                session.as_str(),
                composed.as_ref(),
                adapter.capabilities().system_prompt,
            );
            announcer.emit(&super::prompt::injection_event(
                composed.as_ref(),
                adapter.capabilities().system_prompt,
            ));
            announcer.emit(&super::announce::Event::Nudge {
                from: nudged_short,
                restarted: true,
            });

            nudge_restarts += 1;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "nudge-restart",
                    detail: &format!("{source} handoff at {}", stored.display()),
                },
            );
            writeln!(
                w,
                "zirv ctx exec: nudged ({nudge_restarts}/{}), restarting with a {source} handoff",
                cfg.supervise.max_nudges
            )?;

            let combined = format!("{prompt_text}\n\n{}", note.to_markdown());
            let extra: Vec<String> = user_extra
                .iter()
                .cloned()
                .chain(prompt_args.iter().cloned())
                .collect();
            command = adapter.headless_cmd(&combined, &session, &extra);
            command.current_dir(repo);
            for (key, value) in turn_env_for(&session) {
                command.env(key, value);
            }
            continue;
        }

        if limit_hit {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "limit",
                    score: 100,
                    action: "limit-park",
                    detail: "agent reported a usage limit; parking until the window resets",
                },
            );
            writeln!(
                w,
                "zirv ctx exec: the agent reported a usage limit, parking until the window resets"
            )?;

            pace::wait_for_window(
                w,
                &state,
                &cfg.pace,
                "exec",
                session.as_str(),
                &now_fn,
                &sleep_fn,
                Some(&announcer),
            );

            let Some(prompt_text) = prompt.clone() else {
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit and the original prompt is unknown, so it cannot relaunch. Pass --prompt to enable parking."
                )?;
                session_guard.release();
                return Ok(EXIT_ROT_EXHAUSTED);
            };

            // A park is not a restart: the budget is for rot, not for waiting.
            session = SessionId::new_v4();
            session_guard.refresh_session(session.as_str());
            transcript = derive_transcript(&session);
            // M2: a park mints a new session id, just like a restart, so the
            // injection attribution is re-logged under it rather than only
            // ever naming the first session this run started with.
            super::prompt::log_injection(
                &state,
                "exec",
                session.as_str(),
                composed.as_ref(),
                adapter.capabilities().system_prompt,
            );
            announcer.emit(&super::prompt::injection_event(
                composed.as_ref(),
                adapter.capabilities().system_prompt,
            ));
            // M8: the user's own extra flags survive the relaunch too, not
            // just zirv's own (the system prompt args).
            let extra: Vec<String> = user_extra
                .iter()
                .cloned()
                .chain(prompt_args.iter().cloned())
                .collect();
            command = adapter.headless_cmd(&prompt_text, &session, &extra);
            command.current_dir(repo);
            for (key, value) in turn_env_for(&session) {
                command.env(key, value);
            }
            continue;
        }

        let reason = if rotted { "rot" } else { "timeout" };
        let exhausted_code = if rotted {
            EXIT_ROT_EXHAUSTED
        } else {
            EXIT_TIMEOUT
        };

        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "kill",
                detail: &transcript.display().to_string(),
            },
        );

        let Some(prompt_text) = prompt.clone() else {
            writeln!(
                w,
                "zirv ctx exec: {reason} detected but the original prompt is unknown, so it cannot restart. Pass --prompt to enable restarts."
            )?;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "stand-down",
                    detail: "no prompt available for restart",
                },
            );
            session_guard.release();
            return Ok(exhausted_code);
        };

        if restarts >= max_restarts {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "give-up",
                    detail: "restart budget exhausted",
                },
            );
            writeln!(
                w,
                "zirv ctx exec: {reason} after {restarts} restarts, giving up with exit {exhausted_code}"
            )?;
            session_guard.release();
            return Ok(exhausted_code);
        }

        let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
        let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
        let (note, source) = handoff::distill_or_structural(
            adapter.as_ref(),
            &cfg.handoff.model,
            &ctx,
            Duration::from_secs(cfg.handoff.timeout_secs),
        );
        let stored = handoff::store(&state, repo, session.as_str(), &note)?;
        announcer.emit(&super::announce::Event::Restart {
            style: source.to_string(),
            stored: stored.display().to_string(),
        });

        restarts += 1;
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "restart",
                detail: &format!("{source} handoff at {}", stored.display()),
            },
        );
        writeln!(
            w,
            "zirv ctx exec: {reason} detected, restarting ({restarts}/{max_restarts}) with a {source} handoff"
        )?;

        session = SessionId::new_v4();
        session_guard.refresh_session(session.as_str());
        // The new session writes somewhere new, so the next iteration's watcher
        // must follow it rather than the file the killed child left behind.
        transcript = derive_transcript(&session);
        // M2: README promises injection attribution "at every session
        // start"; a restart mints a new session id, so it needs its own
        // log entry rather than leaving attribution pinned to the first one.
        super::prompt::log_injection(
            &state,
            "exec",
            session.as_str(),
            composed.as_ref(),
            adapter.capabilities().system_prompt,
        );
        announcer.emit(&super::prompt::injection_event(
            composed.as_ref(),
            adapter.capabilities().system_prompt,
        ));
        let combined = format!("{prompt_text}\n\n{}", note.to_markdown());
        // M8: the user's own extra flags survive the restart too, not just
        // zirv's own (the system prompt args) -- this used to be asymmetric.
        let extra: Vec<String> = user_extra
            .iter()
            .cloned()
            .chain(prompt_args.iter().cloned())
            .collect();
        command = adapter.headless_cmd(&combined, &session, &extra);
        command.current_dir(repo);
        for (key, value) in turn_env_for(&session) {
            command.env(key, value);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise_run(
    child: &mut std::process::Child,
    deadline: Instant,
    poll: Duration,
    scorer: &mut score::IncrementalScorer,
    adapter: &dyn adapters::AgentAdapter,
    score_cfg: &super::config::ScoreConfig,
    state: &StateDir,
    server: Option<&signal::SignalServer>,
    session: &str,
    rotted: &mut bool,
    tap: &supervise::OutputTap,
    limit_hit: &mut bool,
    nudged: &mut bool,
    nudges_used: u32,
    max_nudges: u32,
    can_restart: bool,
) -> CtxResult<Outcome> {
    let session_short = super::sessions::short_id(session);
    let mut tick = || {
        if pace::scan_for_limit(
            &tap.try_lines(),
            state,
            session,
            "exec",
            &mut std::io::stderr(),
        ) {
            *limit_hit = true;
            return Tick::Stop("limit");
        }
        if let Some(server) = server
            && let Some(received) = server.try_recv()
            && should_stop_for_signal(&received, session)
        {
            *rotted = true;
            return Tick::Stop("rot");
        }
        // N4: claiming the marker is atomic (`remove_file`), so exactly one
        // observer ever sees `true` -- important even within one process,
        // since a stale marker from a previous cycle must never re-fire.
        // Gracefully stops the child (same `Tick::Stop` shape rot uses) only
        // when a relaunch is actually possible and the consecutive-nudge cap
        // has not been reached; otherwise the marker is still claimed (so it
        // never re-triggers) but the child runs on untouched and the mail
        // stays unread -- `nudge-ignored` in the decision log says why.
        if super::sessions::claim_nudge_marker(state, &session_short) {
            if can_restart && nudges_used < max_nudges {
                *nudged = true;
                return Tick::Stop("nudge");
            }
            let _ = log::append(
                state,
                &log::Decision {
                    ts: now_secs(),
                    session,
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "nudge-ignored",
                    detail: if can_restart {
                        "consecutive nudge cap reached; message left unread"
                    } else {
                        "no prompt available for a nudge relaunch; message left unread"
                    },
                },
            );
            return Tick::Continue;
        }
        // A scoring failure must never kill a healthy run.
        match scorer.poll(adapter, score_cfg) {
            Ok(Some(score)) if score.verdict == Verdict::Restart => {
                *rotted = true;
                Tick::Stop("rot")
            }
            _ => Tick::Continue,
        }
    };
    supervise::supervise_child(child, deadline, poll, &mut tick)
}

pub fn run<W: Write>(args: &ExecArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Runs the fake agent directly, so `exec` supervises a real child whose
    /// transcript path we control through `--transcript`.
    fn fake_agent_command(session: &str) -> Vec<String> {
        vec![
            "sh".to_string(),
            fixture("fake-agent.sh").display().to_string(),
            "-p".to_string(),
            "do the work".to_string(),
            "--session-id".to_string(),
            session.to_string(),
        ]
    }

    fn base_env(state: &std::path::Path) -> HashMap<String, String> {
        [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                fixture("fake-agent.sh").display().to_string(),
            ),
        ]
        .into()
    }

    fn transcript_for(home: &std::path::Path, repo: &std::path::Path, session: &str) -> PathBuf {
        home.join(".claude/projects")
            .join(crate::commands::ctx::adapters::claude::project_slug(repo))
            .join(format!("{session}.jsonl"))
    }

    #[test]
    fn prompt_extraction_finds_the_dash_p_argument() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
        ];
        assert_eq!(extract_prompt(&cmd), Some("fix the bug".to_string()));
    }

    #[test]
    fn prompt_extraction_handles_print_and_positional_forms() {
        assert_eq!(
            extract_prompt(&[
                "claude".to_string(),
                "--print".to_string(),
                "go".to_string()
            ]),
            Some("go".to_string())
        );
        assert_eq!(
            extract_prompt(&["codex".to_string(), "exec".to_string(), "go".to_string()]),
            Some("go".to_string())
        );
    }

    #[test]
    fn prompt_extraction_gives_up_rather_than_guessing() {
        assert_eq!(
            extract_prompt(&["claude".to_string(), "-p".to_string()]),
            None
        );
        assert_eq!(
            extract_prompt(&[
                "claude".to_string(),
                "--resume".to_string(),
                "abc".to_string()
            ]),
            None
        );
        assert_eq!(extract_prompt(&[]), None);
    }

    /// M8: only the prompt and `--session-id` (both regenerated fresh on
    /// every restart) are stripped; everything else the operator passed
    /// survives.
    #[test]
    fn extra_launch_flags_strips_only_the_prompt_and_session_id() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn extra_launch_flags_is_empty_when_the_command_is_only_prompt_and_session_id() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
        ];
        assert!(extra_launch_flags(&cmd, 1, None).is_empty());
    }

    /// A markdown bullet list is an ordinary prompt. Reading it as a flag left
    /// the `-p` pair in the operator's flags, so every restart passed the
    /// prompt twice: once with the handoff, once without, and the second one
    /// won.
    #[test]
    fn a_prompt_that_starts_with_a_dash_is_still_stripped_from_the_restart_flags() {
        let prompt = "- fix the failing tests\n- then run cargo fmt";
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            prompt.to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, Some(prompt)),
            vec!["--model".to_string(), "opus".to_string()],
            "the prompt zirv already holds is recognised by value, not by shape"
        );
    }

    /// Without the prompt to compare against, a value shaped like a flag still
    /// reads as one -- but only the flag is dropped, never the token after it,
    /// which belongs to the operator.
    #[test]
    fn a_bare_prompt_flag_drops_itself_and_keeps_what_follows() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "--verbose".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None),
            vec!["--verbose".to_string()]
        );
    }

    /// `headless_cmd` rebuilds the program invocation on every relaunch, so a
    /// launcher in front of the agent (or a positional prompt) must not come
    /// back as a stray argument the agent reads as a second prompt.
    #[test]
    fn the_program_invocation_is_never_carried_into_the_restart_flags() {
        let via_npx = vec![
            "npx".to_string(),
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
        ];
        assert!(
            extra_launch_flags(&via_npx, 1, Some("task")).is_empty(),
            "the launcher's own argument is part of the invocation, not a flag"
        );

        // `agent_bin = "/usr/bin/env claude"`: the adapter reports a prefix of
        // two, because that is how many tokens it spends before the flags.
        let via_env = vec![
            "/usr/bin/env".to_string(),
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
        ];
        assert!(extra_launch_flags(&via_env, 2, Some("task")).is_empty());

        let positional = vec!["claude".to_string(), "task".to_string()];
        assert!(extra_launch_flags(&positional, 1, Some("task")).is_empty());
    }

    /// A restart exists to escape the conversation that rotted. Every spelling
    /// that would pin it back to that conversation has to go.
    #[test]
    fn nothing_that_pins_the_launch_to_the_dead_session_survives_a_restart() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
            "--session-id=OLD".to_string(),
            "--continue".to_string(),
            "--resume".to_string(),
            "abc".to_string(),
            "--fork-session".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, Some("task")),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    /// `--resume` with a flag after it took no value, so swallowing the next
    /// token would eat one of the operator's own flags.
    #[test]
    fn a_valueless_resume_does_not_swallow_the_next_flag() {
        let cmd = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn extra_launch_flags_keeps_everything_when_there_is_no_prompt_or_session_id() {
        let cmd = vec![
            "codex".to_string(),
            "--model".to_string(),
            "gpt".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None),
            vec!["--model".to_string(), "gpt".to_string()]
        );
    }

    #[test]
    fn a_healthy_run_exits_with_the_childs_own_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "11111111-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    #[test]
    fn a_failing_child_propagates_its_exit_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "22222222-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 3);
    }

    /// `FAKE_AGENT_MODE` applies to every invocation, so both the original child
    /// and the restarted one rot and the budget runs out.
    #[test]
    fn a_rotted_run_is_killed_restarted_and_capped() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "33333333-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "the caller applies its own policy after the budget is spent"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verb\":\"exec\""), "got {log}");
        assert!(
            log.contains("\"action\":\"restart\""),
            "a restart was attempted: {log}"
        );
        assert!(
            log.contains("\"action\":\"give-up\""),
            "and then it stopped: {log}"
        );

        let handoffs = state.join("handoffs");
        let stored: Vec<_> = walk_md(&handoffs);
        assert!(
            !stored.is_empty(),
            "a handoff is written before each restart"
        );
    }

    fn walk_md(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_md(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                found.push(path);
            }
        }
        found
    }

    fn transcripts_in(home: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(dirs) = std::fs::read_dir(home.join(".claude/projects")) else {
            return found;
        };
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for file in files.flatten() {
                if file.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    found.push(file.path());
                }
            }
        }
        found
    }

    /// The restarted child is a new session writing to a new transcript, so
    /// supervision must follow it there. If the watcher kept polling the killed
    /// child's rotted file, this healthy second child would be killed too and
    /// the run would exit 75 instead of 0.
    #[test]
    fn a_restart_supervises_the_new_sessions_transcript() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "88888888-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        // First child rots, second is healthy.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_TURNS", "12");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the healthy restarted child must be allowed to finish"
        );

        let found = transcripts_in(&home);
        assert_eq!(found.len(), 2, "one transcript per session: {found:?}");
        let first = transcript_for(&home, tmp.path(), session);
        assert!(
            found.contains(&first),
            "the original session's transcript: {found:?}"
        );
        assert!(
            found.iter().any(|p| *p != first),
            "the restarted session wrote its own transcript: {found:?}"
        );
    }

    #[test]
    fn a_run_with_no_discoverable_prompt_refuses_to_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "44444444-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            // Keep the child alive past the first scoring tick so rot is seen.
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "rot was detected but no restart was possible"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("cannot restart"),
            "say why supervision stood down: {text}"
        );
    }

    /// The old warning about a missing prompt only ever surfaced once a
    /// restart was already needed (see `a_run_with_no_discoverable_prompt_
    /// refuses_to_restart` above), so a healthy run that never rots gave the
    /// operator no signal at all that restarts were a dead end for this
    /// invocation. It must appear upfront, regardless of whether the run
    /// ever actually needs to restart.
    #[test]
    fn an_upfront_warning_appears_even_when_the_run_never_needs_to_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "eeeeeeee-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "a healthy run that never rots must still succeed"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("--prompt") && text.to_lowercase().contains("restart"),
            "an upfront warning must appear even though this run never needed to restart: {text}"
        );
    }

    #[test]
    fn an_empty_command_is_rejected() {
        let tmp = crate::commands::ctx::testenv::repo();
        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            timeout_secs: None,
            simple: false,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to supervise");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    use crate::commands::ctx::rot::Verdict;
    use crate::commands::ctx::signal::TurnSignal;

    fn signal_with(verdict: Verdict, score: u32) -> TurnSignal {
        TurnSignal {
            session_id: "s".to_string(),
            turn: 4,
            score,
            verdict,
            transcript_path: None,
        }
    }

    #[test]
    fn only_a_restart_signal_stops_the_run() {
        assert!(should_stop_for_signal(
            &signal_with(Verdict::Restart, 95),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Compact, 65),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Advise, 45),
            "s"
        ));
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Healthy, 0),
            "s"
        ));
    }

    /// The socket path is derived from the first eight hex characters of a
    /// session id, so a stale hook or a neighbouring run can reach it. Killing
    /// a healthy child on someone else's verdict is the failure to avoid.
    #[test]
    fn a_verdict_about_another_session_is_ignored() {
        assert!(!should_stop_for_signal(
            &signal_with(Verdict::Restart, 95),
            "a-different-session"
        ));
    }

    /// Every restart is a new session, and the hook inside it reports whatever
    /// `ZIRV_CTX_SESSION` says. Leave that pinned to the dead session's id and
    /// the session check above rejects every signal the restart produces.
    #[test]
    fn a_restarted_child_is_told_its_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "cccccccc-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));
        let seen = tmp.path().join("sessions.txt");

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SESSION_ENV_LOG", &seen);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_TURNS", "12");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SESSION_ENV_LOG");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }
        assert_eq!(code.expect("runs"), 0);

        let logged: Vec<String> = std::fs::read_to_string(&seen)
            .expect("the children recorded their session env")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(logged.len(), 2, "one line per child: {logged:?}");
        assert_eq!(logged[0], session, "the first child owns the given id");

        let first = transcript_for(&home, tmp.path(), session);
        let restarted = transcripts_in(&home)
            .into_iter()
            .find(|path| *path != first)
            .expect("the restarted child wrote its own transcript");
        let restarted_session = restarted
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("session id from the transcript name");
        assert_eq!(
            logged[1], restarted_session,
            "the restart must export the new session id, not the dead one's"
        );
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "55555555-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(1),
            simple: false,
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_TIMEOUT);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline must not wait for the child"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"timeout\""), "got {log}");
    }

    #[cfg(unix)]
    #[test]
    fn the_child_is_told_where_the_socket_is() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "66666666-2222-4333-8444-555555555555";
        let env = base_env(&state);
        let marker = tmp.path().join("socket-env.txt");

        // A child that records the socket env it inherited, then exits.
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s' \"$ZIRV_CTX_SOCKET\" > {}; exit 0",
                marker.display()
            ),
        ];

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");

        let seen = std::fs::read_to_string(&marker).expect("marker written");
        assert!(seen.ends_with(".sock"), "socket path exported: {seen}");
        assert!(seen.contains("66666666"), "per-session socket: {seen}");
    }

    #[test]
    fn an_unbindable_socket_does_not_stop_the_run() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "77777777-2222-4333-8444-555555555555";
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("x".repeat(120));
        let mut env = base_env(&long_state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0, "polling still supervises the run");
    }

    use crate::commands::ctx::window::{self, UsageWindows, Window};

    fn store_collector(state_dir: &std::path::Path, percent: f64, resets_in: u64) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let now = crate::commands::ctx::state::now_secs();
        window::store(
            &state,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: percent,
                    resets_at: now + resets_in,
                    observed_at: now,
                }),
                seven_day: None,
            },
        )
        .expect("store collector state");
    }

    #[test]
    fn a_limit_hit_parks_and_relaunches_without_spending_the_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "99999999-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        // A reset one second out plus no jitter keeps the park short; the point
        // is that it parks and relaunches, not how long it waits.
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_FALLBACK_SECS".to_string(), "1".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());

        // First child hits the limit, second runs clean.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // Zero budget: a limit hit must park even with no restarts allowed,
            // because a park is not a restart.
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the relaunched child finished cleanly, so exec exits with its code"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "a park must not consume the restart budget: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "the relaunch is a new session with its own transcript"
        );
    }

    /// Wording that only loosely resembles a usage-limit notice leaves a
    /// breadcrumb in the decision log and changes nothing else: the run is not
    /// parked, and its exit code is still the child's own.
    #[test]
    fn a_loose_limit_wording_is_noted_without_parking_the_run() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "77777777-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "drift");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0, "a breadcrumb is not a park");
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"limit-wording-drift\""),
            "the drift must be recorded: {log}"
        );
        assert!(
            !log.contains("\"action\":\"limit-park\""),
            "and it must never park a healthy run: {log}"
        );
    }

    #[test]
    fn an_exhausted_window_delays_the_first_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "aaaaaaaa-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        store_collector(&state, 100.0, 1);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0, "a pause is never an exit");
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(1),
            "it should have waited before spawning"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"pace-wait\""), "got {log}");
    }

    #[test]
    fn a_restart_relaunches_with_the_system_prompt_too() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "cccccccc-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("--append-system-prompt"),
            "the restarted child must carry the prompt too: {argv}"
        );
    }

    /// M8: a restart used to rebuild the headless command from scratch with
    /// only zirv's own added flags (the system prompt), silently dropping any
    /// extra flag the operator themselves had passed after `--`. Only lines
    /// carrying `--session-id` are real agent invocations (a `--help` probe,
    /// if any ran, never gets one), so filtering on it keeps this assertion
    /// meaningful regardless of what else shares the log.
    #[test]
    fn a_restart_preserves_the_users_own_extra_flags_not_just_zirvs() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "12121212-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut command = fake_agent_command(session);
        command.push("--zzz-custom-flag".to_string());
        command.push("custom-value".to_string());
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        let invocations: Vec<&str> = argv
            .lines()
            .filter(|line| line.contains("--session-id"))
            .collect();
        assert_eq!(
            invocations.len(),
            2,
            "one real invocation per child: {argv:?}"
        );
        for line in &invocations {
            assert!(
                line.contains("--zzz-custom-flag") && line.contains("custom-value"),
                "the user's own extra flag must survive every restart, not just the first spawn: {argv}"
            );
        }
    }

    /// M2: README promises that "whether a prompt was injected, and from
    /// which layers, is recorded in the decision log at every session
    /// start". A restart mints a new session id, so its own attribution
    /// entry must be logged under that id too, not only the first session's.
    #[test]
    fn injection_is_logged_again_for_each_restarts_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "ffffffff-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        let injected_sessions: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("\"action\":\"prompt-injected\""))
            .filter_map(|l| {
                let key = "\"session\":\"";
                let start = l.find(key)? + key.len();
                let end = l[start..].find('"')? + start;
                Some(&l[start..end])
            })
            .collect();
        assert_eq!(
            injected_sessions.len(),
            2,
            "one attribution entry per actual session id, including the restart: {log}"
        );
        assert_ne!(
            injected_sessions[0], injected_sessions[1],
            "the restart mints a new session id and must be logged under it: {log}"
        );
    }

    /// T7: unread mail addressed to this session's agent is folded into the
    /// composed system prompt at launch, the same way the repo layer is.
    #[test]
    fn unread_mail_is_delivered_into_the_launch_system_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "abababab-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "the mail must reach the launch's composed prompt: {argv}"
        );
        assert!(
            argv.contains("another agent session"),
            "labeled as mail, not as an operator instruction: {argv}"
        );
    }

    /// B3: `mail.enabled = false` must gate delivery at every seam that folds
    /// mail into a composed prompt, not just `send`/`inbox`.
    #[test]
    fn disabled_mail_is_not_delivered_into_a_headless_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "eeeeeeee-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_MAIL".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            !argv.contains("heads up: the webhook route moved"),
            "mail.enabled = false must gate delivery, not just send/inbox: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(
            unread.len(),
            1,
            "a delivery that never happened must not consume the message either"
        );
    }

    /// S3: mail delivered into a launch prompt is consumed right after, so a
    /// later launch does not redeliver it.
    #[test]
    fn delivered_mail_is_not_delivered_a_second_time() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let session1 = "abababab-2222-4333-8444-555555555555";
        let argv_log1 = tmp.path().join("argv1.log");
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log1);
        }
        let args1 = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session1.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session1)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session1),
        };
        let mut out1 = Vec::new();
        let code1 = run_with(&args1, &mut out1, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code1.expect("first launch runs"), 0);
        let argv1 = std::fs::read_to_string(&argv_log1).expect("argv recorded");
        assert!(
            argv1.contains("heads up: the webhook route moved"),
            "the first launch must see the mail: {argv1}"
        );

        let session2 = "cdcdcdcd-2222-4333-8444-555555555555";
        let argv_log2 = tmp.path().join("argv2.log");
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log2);
        }
        let args2 = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session2.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session2)),
            prompt: Some("do more work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session2),
        };
        let mut out2 = Vec::new();
        let code2 = run_with(&args2, &mut out2, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code2.expect("second launch runs"), 0);
        let argv2 = std::fs::read_to_string(&argv_log2).expect("argv recorded");
        assert!(
            !argv2.contains("heads up: the webhook route moved"),
            "the mail was already delivered once and must not be redelivered: {argv2}"
        );
    }

    /// Item 3 (regression): a launch that never actually spawns must not
    /// consume the mail it would have delivered -- no session ever saw it,
    /// so it must stay unread for whichever later invocation actually gets
    /// one running. The old ordering consumed mail immediately after
    /// composing the prompt, well before `spawn_tapped` (and the pacing
    /// gate ahead of it) ever ran.
    #[test]
    fn mail_is_not_consumed_when_the_launch_fails_before_spawning() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "must stay unread".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let session = "12312312-2222-4333-8444-555555555555";
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            // `adapters::select` still resolves and readies "claude" (via
            // `ZIRV_CTX_AGENT_BIN` in `base_env`, unaffected by this); only
            // the actual spawn of *this* program has to fail, deterministically
            // and without depending on any real binary's own behavior.
            command: vec![
                "zirv-test-binary-that-does-not-exist-anywhere".to_string(),
                "-p".to_string(),
                "do the work".to_string(),
                "--session-id".to_string(),
                session.to_string(),
            ],
        };
        let mut out = Vec::new();
        let result = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert!(result.is_err(), "the launch must fail to spawn: {result:?}");

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(
            unread.len(),
            1,
            "a launch that never spawned must not have consumed the mail"
        );
    }

    /// S3: a consume failure (e.g. `read/` cannot be created) must not sink
    /// the launch -- the mail already reached the prompt either way.
    #[test]
    fn a_failed_consume_does_not_stop_the_launch() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "ffffffff-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        // Block `mail::consume`'s own `read/` directory creation by putting
        // an ordinary file where it needs a directory: a deterministic way
        // to force the consume step to fail without racing a real
        // filesystem deletion mid-flight.
        std::fs::write(state.mail().join(&slug).join("read"), b"not a directory")
            .expect("write blocker");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("a failed consume must not fail the launch"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "the mail still had to reach the prompt even though consuming it afterward failed: {argv}"
        );
    }

    /// I2: a user's own --append-system-prompt inside the `--` command must
    /// not be silently discarded by zirv's own occurrence of the same flag.
    #[test]
    fn a_users_own_append_system_prompt_is_merged_into_the_first_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "dddddddd-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut command = fake_agent_command(session);
        command.push("--append-system-prompt".to_string());
        command.push("always answer in Danish".to_string());
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            simple: false,
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert_eq!(
            argv.matches("--append-system-prompt").count(),
            1,
            "exactly one flag must reach the agent: {argv}"
        );
        assert!(
            argv.contains("always answer in Danish"),
            "the user's own instruction must survive: {argv}"
        );
        assert!(
            argv.contains("zirv session conventions"),
            "zirv's own layer is still present: {argv}"
        );
    }

    /// Shared with `zirv ctx agent` (agent.rs) and script `agent:` steps
    /// (agent_command.rs), which both delegate to this supervisor and want
    /// the same wording for the same two outcomes: the supervisor's own exit
    /// codes read as outcomes, not agent failures.
    #[test]
    fn describe_exit_names_the_supervisors_own_outcomes() {
        assert!(describe_exit(EXIT_ROT_EXHAUSTED).contains("restart budget"));
        assert!(describe_exit(EXIT_TIMEOUT).contains("wall-clock timeout"));
        assert_eq!(describe_exit(1), "exited with code 1");
    }

    #[test]
    fn a_healthy_window_adds_no_delay() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "bbbbbbbb-2222-4333-8444-555555555555";
        let env = base_env(&state);
        store_collector(&state, 5.0, 3600);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(!log.contains("pace-wait"), "nothing to wait for: {log}");
    }

    // N4: `zirv ctx nudge` restarting a headless worker.
    //
    // Every test here drives a real `run_with` call whose first agent
    // invocation hangs (`FAKE_AGENT_MODE_FILE` starting with "hang") and is
    // nudged from a background thread once its transcript is up. Like every
    // other test in this module that spawns `sh`/`fake-agent.sh`, these are
    // blocked on Windows by the pre-existing os-193 spawn issue (see this
    // module's other `sh`-spawning tests); written to the same standard the
    // rest of this suite holds regardless.

    /// Polls `path` until it has at least `n` lines or `timeout` elapses,
    /// returning whatever was there either way -- the same "best effort,
    /// bounded wait" shape `run_loop.rs`'s own synchronized tests use via a
    /// marker file, adapted here to a growing log instead of a touch-once
    /// marker since more than one invocation is expected.
    fn wait_for_lines(path: &std::path::Path, n: usize, timeout: Duration) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                if lines.len() >= n {
                    return lines;
                }
            }
            if Instant::now() >= deadline {
                return std::fs::read_to_string(path)
                    .map(|t| t.lines().map(|l| l.to_string()).collect())
                    .unwrap_or_default();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Nudges whichever session is currently live. `exec` (like `loop`)
    /// keeps exactly one registry record at a time, refreshed on every
    /// restart or park, so an empty prefix (`starts_with("")` is always
    /// true) always resolves to the run this test is driving without the
    /// test needing to know that session's exact id.
    fn nudge_live_session(state_dir: &std::path::Path, repo: &std::path::Path, message: &str) {
        let env: HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.display().to_string(),
        )]
        .into();
        let args = crate::commands::ctx::sessions::NudgeArgs {
            prefix: String::new(),
            message: Some(message.to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        crate::commands::ctx::sessions::run_nudge_with(
            &args,
            &mut out,
            repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("nudge the live session");
    }

    #[test]
    fn a_headless_worker_stops_at_the_next_poll_and_relaunches_with_the_guidance() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session_log = tmp.path().join("session.log");
        let session = "10101010-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
            std::env::set_var("FAKE_AGENT_SESSION_ENV_LOG", &session_log);
        }

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            let lines = wait_for_lines(&session_log_for_writer, 1, Duration::from_secs(5));
            if lines.is_empty() {
                return;
            }
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "switch to the new failing test",
            );
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
            std::env::remove_var("FAKE_AGENT_SESSION_ENV_LOG");
        }
        assert_eq!(
            code.expect("the second (healthy) launch finishes the run"),
            0
        );

        let sessions = wait_for_lines(&session_log, 2, Duration::from_millis(1));
        assert_eq!(
            sessions.len(),
            2,
            "exactly one relaunch: the nudge, then a clean exit"
        );
        assert_ne!(
            sessions[0], sessions[1],
            "the relaunch mints a fresh session id"
        );

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("switch to the new failing test"),
            "the nudge's guidance must reach the relaunch's composed prompt: {argv}"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
    }

    /// N4: a nudge-driven restart must never touch the rot restart budget --
    /// with `max_restarts: 0`, an ordinary rot or timeout restart would
    /// immediately "give up"; a nudge restart must succeed anyway, and the
    /// normal rot-restart machinery (`"action":"restart"`, a `"rot"` or
    /// `"timeout"` verdict) must never fire at all.
    #[test]
    fn a_nudge_restart_does_not_spend_the_rot_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "20202020-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SESSION_ENV_LOG", &session_log);
        }

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            let lines = wait_for_lines(&session_log_for_writer, 1, Duration::from_secs(5));
            if lines.is_empty() {
                return;
            }
            nudge_live_session(&state_for_writer, &repo_for_writer, "keep going");
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // Zero rot-restart budget: proves the nudge restart below is not
            // drawing from it.
            max_restarts: Some(0),
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SESSION_ENV_LOG");
        }
        assert_eq!(
            code.expect("a nudge restart with zero rot budget must still succeed"),
            0
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"restart\""),
            "the ordinary rot-restart action must never fire: {log}"
        );
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "zero budget only matters to rot/timeout, which never triggered: {log}"
        );
        assert!(
            !log.contains("\"verdict\":\"rot\"") && !log.contains("\"verdict\":\"timeout\""),
            "nothing here rotted or timed out: {log}"
        );
    }

    /// N4: a nudge restart carries a handoff forward exactly like a rot or
    /// timeout restart does -- distilled or structural, stored under the old
    /// session, and named in the decision log detail the same way
    /// `"{source} handoff at {path}"` already reads for the ordinary path.
    #[test]
    fn a_nudge_restart_carries_a_handoff_forward_like_every_other_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "30303030-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SESSION_ENV_LOG", &session_log);
        }

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            let lines = wait_for_lines(&session_log_for_writer, 1, Duration::from_secs(5));
            if lines.is_empty() {
                return;
            }
            nudge_live_session(&state_for_writer, &repo_for_writer, "keep going");
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SESSION_ENV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        assert!(
            crate::commands::ctx::handoff::latest_for_repo(&state, tmp.path())
                .expect("handoff lookup")
                .is_some(),
            "a nudge restart must distill and store a handoff, like every other restart"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        let nudge_restart_line = log
            .lines()
            .find(|l| l.contains("\"action\":\"nudge-restart\""))
            .unwrap_or_else(|| panic!("no nudge-restart entry: {log}"));
        assert!(
            nudge_restart_line.contains("handoff at"),
            "names the handoff the same way an ordinary restart does: {nudge_restart_line}"
        );
    }

    /// N4: `cfg.supervise.max_nudges` caps consecutive nudge restarts. Past
    /// the cap the marker is still claimed (so it does not keep re-firing)
    /// but nothing is stopped or relaunched, and the nudge's own mail stays
    /// unread -- still visible via `zirv ctx inbox` -- rather than being
    /// silently dropped.
    #[test]
    fn consecutive_nudge_restarts_are_capped_and_the_message_is_left_unread() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "40404040-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert("ZIRV_CTX_MAX_NUDGES".to_string(), "1".to_string());

        // Three potential runs scripted; only two are ever expected to
        // start (the first nudge restarts once, the second is ignored, and
        // the second run's own hang has to end some other way -- the
        // `timeout_secs` below, with a zero rot budget, is what ends it).
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SESSION_ENV_LOG", &session_log);
        }

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || -> Vec<String> {
            let first = wait_for_lines(&session_log_for_writer, 1, Duration::from_secs(5));
            if first.is_empty() {
                return Vec::new();
            }
            nudge_live_session(&state_for_writer, &repo_for_writer, "first nudge, honored");

            let second = wait_for_lines(&session_log_for_writer, 2, Duration::from_secs(5));
            if second.len() < 2 {
                return second;
            }
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "second nudge, should be ignored",
            );
            second
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            // Short enough that the second (ignored-nudge) hang ends the
            // run on its own once the cap has been proven, rather than
            // hanging the test forever.
            timeout_secs: Some(3),
            simple: false,
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        let sessions = writer.join().expect("writer thread");
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SESSION_ENV_LOG");
        }
        assert_eq!(
            code.expect("runs"),
            EXIT_TIMEOUT,
            "the second hang is never nudged into relaunching again, so it eventually times out \
             with no rot budget left to restart on"
        );

        let all_sessions = wait_for_lines(&session_log, 2, Duration::from_millis(1));
        assert_eq!(
            all_sessions.len(),
            2,
            "exactly one relaunch (the first nudge); the second was ignored: {all_sessions:?}"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("\"action\":\"nudge-restart\""))
                .count(),
            1,
            "only the first nudge restarts: {log}"
        );
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("\"action\":\"nudge-ignored\""))
                .count(),
            1,
            "the second is claimed but ignored, not silently dropped: {log}"
        );

        // The second nudge's own mail must still be sitting there, unread.
        let second_short = sessions
            .get(1)
            .map(|raw| crate::commands::ctx::sessions::short_id(raw))
            .expect("the second session started");
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let unread = crate::commands::ctx::mail::list(&state, &slug, None, Some(&second_short))
            .expect("list");
        assert_eq!(
            unread.len(),
            1,
            "the ignored nudge's mail is left unread, still visible via `zirv ctx inbox`: {unread:?}"
        );
        assert_eq!(unread[0].1.body, "second nudge, should be ignored");
    }
}
