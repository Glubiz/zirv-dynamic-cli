use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::adapters::AgentAdapter;
use super::config::{CtxConfig, EnvLookup, ScoreConfig, env_from_process};
use super::event::{SessionId, SessionRef, input_hash};
use super::rot::{self, RotState, Score};
use super::state::StateDir;
use super::supervise::Watcher;
use super::{CtxResult, adapters};

#[derive(Debug, clap::Args)]
pub struct ScoreArgs {
    /// Path to the agent transcript (JSONL).
    #[arg(long)]
    pub transcript: PathBuf,
    /// Adapter name: claude or codex. Defaults to config, then claude.
    #[arg(long)]
    pub agent: Option<String>,
}

/// Read a whole transcript, parse it with the selected adapter, score it. The
/// reference every incremental pass has to agree with.
fn full_score(
    adapter: &dyn AgentAdapter,
    transcript: &Path,
    cfg: &ScoreConfig,
) -> CtxResult<Score> {
    let jsonl = std::fs::read_to_string(transcript)
        .map_err(|e| format!("{}: {e}", transcript.display()))?;
    Ok(rot::score_events(
        &adapter.parse_events(&jsonl),
        adapter.capabilities(),
        cfg,
    ))
}

/// One-shot scoring, used by the `score` verb itself: no state is kept, so the
/// whole transcript is parsed every time.
pub fn score_transcript(
    transcript: &Path,
    agent: Option<&str>,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<Score> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(agent.or(cfg.agent.as_deref()), &[], &cfg)?;
    full_score(adapter.as_ref(), transcript, &cfg.score)
}

/// Folds a growing transcript into a `RotState` so each pass costs the bytes
/// appended since the last one rather than the whole session. Correctness is
/// never traded for that: whenever the `Watcher` reports the file was
/// rewritten, or the state was folded under different rules, it is thrown away
/// and rebuilt from what the file says now.
pub struct IncrementalScorer {
    transcript: PathBuf,
    watcher: Watcher,
    state: Option<RotState>,
}

impl IncrementalScorer {
    pub fn new(transcript: PathBuf) -> Self {
        Self {
            watcher: Watcher::new(transcript.clone()),
            transcript,
            state: None,
        }
    }

    /// Resumes from a checkpoint a previous process wrote.
    fn resuming(transcript: PathBuf, offset: u64, consumed: u64, state: RotState) -> Self {
        Self {
            watcher: Watcher::resuming(transcript.clone(), offset, consumed),
            transcript,
            state: Some(state),
        }
    }

    pub fn position(&self) -> (u64, u64) {
        self.watcher.position()
    }

    fn state(&self) -> Option<&RotState> {
        self.state.as_ref()
    }

    /// `None` when the transcript has not changed since the last poll, which
    /// leaves the caller's previous verdict standing.
    pub fn poll(
        &mut self,
        adapter: &dyn AgentAdapter,
        cfg: &ScoreConfig,
    ) -> CtxResult<Option<Score>> {
        let Some(appended) = self.watcher.read_appended()? else {
            return Ok(None);
        };
        if appended.restarted || self.state.as_ref().is_none_or(|s| !s.built_for(cfg)) {
            self.state = RotState::new(cfg);
        }
        let Some(state) = self.state.as_mut() else {
            // An unbounded window has no bounded state to fold into.
            return full_score(adapter, &self.transcript, cfg).map(Some);
        };
        state.feed_all(&adapter.parse_events(&appended.lines));

        // The line the agent is still writing counts towards this pass's score
        // -- a full parse would see it too -- but is never committed to the
        // state, because the next poll reads it again, complete.
        if appended.partial.is_empty() {
            return Ok(state.score(adapter.capabilities(), cfg));
        }
        let mut with_partial = state.clone();
        with_partial.feed_all(&adapter.parse_events(&appended.partial));
        Ok(with_partial.score(adapter.capabilities(), cfg))
    }
}

/// Bumped whenever the checkpoint or `RotState` changes shape, so an older
/// file is ignored and rebuilt instead of misread.
const CHECKPOINT_VERSION: u32 = 1;

/// What a fresh process needs to carry on folding where the last one stopped.
#[derive(Debug, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    /// The transcript this state describes: a checkpoint that outlived its
    /// session must never be applied to a different one.
    transcript: String,
    /// Adapter, capabilities and score config the state was folded under.
    fingerprint: u64,
    offset: u64,
    consumed: u64,
    state: RotState,
}

/// Everything outside the transcript that decides what the same bytes score
/// to. Any change to it rebuilds rather than reusing state folded under rules
/// that no longer apply.
fn fingerprint(adapter: &dyn AgentAdapter, cfg: &ScoreConfig) -> u64 {
    input_hash(&format!(
        "{CHECKPOINT_VERSION}|{}|{:?}|{cfg:?}",
        adapter.name(),
        adapter.capabilities()
    ))
}

/// One file per transcript, named after a hash of its path: the path itself
/// carries the session id and is far too long to be a filename.
fn checkpoint_path(state: &StateDir, transcript: &Path) -> PathBuf {
    state.scoring().join(format!(
        "{:016x}.json",
        input_hash(&transcript.display().to_string())
    ))
}

/// `None` on any doubt at all -- unreadable, corrupt, a different schema
/// version, a different transcript, different scoring rules, or an offset that
/// no longer fits the file -- which sends the caller back to a full parse.
fn load_checkpoint(
    path: &Path,
    transcript: &Path,
    fingerprint: u64,
    cfg: &ScoreConfig,
) -> Option<Checkpoint> {
    let checkpoint: Checkpoint = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let usable = checkpoint.version == CHECKPOINT_VERSION
        && checkpoint.transcript == transcript.display().to_string()
        && checkpoint.fingerprint == fingerprint
        && checkpoint.state.built_for(cfg)
        && checkpoint.offset <= std::fs::metadata(transcript).ok()?.len();
    usable.then_some(checkpoint)
}

/// Best-effort: a checkpoint that cannot be written costs the next pass a full
/// parse, which is exactly what happened before there were checkpoints.
fn save_checkpoint(path: &Path, transcript: &Path, fingerprint: u64, scorer: &IncrementalScorer) {
    let Some(state) = scorer.state() else {
        return;
    };
    let (offset, consumed) = scorer.position();
    let Ok(json) = serde_json::to_string(&Checkpoint {
        version: CHECKPOINT_VERSION,
        transcript: transcript.display().to_string(),
        fingerprint,
        offset,
        consumed,
        state: state.clone(),
    }) else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    let _ = super::state::create_private_dir_all(dir);
    // Renamed into place so a hook killed mid-write leaves the previous
    // checkpoint intact rather than a truncated one.
    let staged = dir.join(format!("{}.tmp", std::process::id()));
    if super::state::write_private(&staged, &json).is_ok() {
        let _ = std::fs::rename(&staged, path);
    }
    // One checkpoint per transcript, and transcripts are never reused, so
    // without this the directory grows for the life of the machine.
    super::state::prune_to_newest(dir, super::state::KEEP_NEWEST);
}

/// The same score `score_transcript` returns, reached by folding only the
/// bytes appended since the previous call for this transcript. Used by the
/// Stop hook, which is a fresh process on every turn, so its state lives in a
/// private file under the state dir. Every failure degrades to a full parse.
pub fn score_transcript_cached(
    transcript: &Path,
    agent: Option<&str>,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<Score> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(agent.or(cfg.agent.as_deref()), &[], &cfg)?;
    let Ok(state_dir) = StateDir::resolve(env) else {
        return full_score(adapter.as_ref(), transcript, &cfg.score);
    };
    score_with_checkpoint(&state_dir, transcript, adapter.as_ref(), &cfg.score)
}

/// The body of [`score_transcript_cached`], against a state dir the caller
/// already has. Split out so the dashboard's [`cached_score`] reaches the same
/// incremental fold without re-resolving the state dir from the environment.
fn score_with_checkpoint(
    state_dir: &StateDir,
    transcript: &Path,
    adapter: &dyn AgentAdapter,
    cfg: &ScoreConfig,
) -> CtxResult<Score> {
    let path = checkpoint_path(state_dir, transcript);
    let fingerprint = fingerprint(adapter, cfg);
    let mut scorer = match load_checkpoint(&path, transcript, fingerprint, cfg) {
        Some(checkpoint) => IncrementalScorer::resuming(
            transcript.to_path_buf(),
            checkpoint.offset,
            checkpoint.consumed,
            checkpoint.state,
        ),
        None => IncrementalScorer::new(transcript.to_path_buf()),
    };

    // A poll that reports nothing new cannot be answered from a checkpoint
    // alone (an unreadable or empty transcript lands here too), so it falls
    // back rather than guessing.
    let Ok(Some(score)) = scorer.poll(adapter, cfg) else {
        return full_score(adapter, transcript, cfg);
    };
    save_checkpoint(&path, transcript, fingerprint, &scorer);
    Ok(score)
}

/// What a transcript looked like when its score was last computed. `mtime`
/// alone can miss an in-place rewrite inside one filesystem clock tick and
/// `len` alone misses an equal-length one, so the pair is the key -- both come
/// out of the single `metadata` call the fast path is allowed to make.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
}

fn stamp_of(transcript: &Path) -> Option<TranscriptStamp> {
    let meta = std::fs::metadata(transcript).ok()?;
    Some(TranscriptStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

#[derive(Debug, Clone)]
struct CachedScore {
    /// Where this session's transcript was resolved to. Kept even when
    /// nothing has been written there yet: resolving it is the expensive part
    /// (`ClaudeAdapter::transcript_path` walks the agent's projects tree when
    /// its slug rule misses), and a stat against a path that does not exist
    /// costs nothing.
    transcript: PathBuf,
    /// The stamp that was actually scored, and its score. `None` until the
    /// transcript first becomes readable.
    scored: Option<(TranscriptStamp, u32)>,
    /// Polls answered off this path since it was resolved, counted only while
    /// the transcript is missing -- see [`RESOLVE_RETRY_POLLS`].
    polls_since_resolve: u32,
}

/// How many polls a session whose transcript has not appeared yet reuses its
/// resolved path before resolving again. Resolving every poll would put a
/// directory walk per pane on a once-a-second render path; never resolving
/// again would strand the rare session whose transcript lands somewhere the
/// first resolution did not predict. At the dashboard's refresh rate this is
/// about ten seconds.
const RESOLVE_RETRY_POLLS: u32 = 10;

/// Process-local, keyed by session id. `OnceLock` rather than a `lazy_static`
/// dependency, and a poisoned lock is recovered with `into_inner` (the same
/// thing `wrap` does with its stdout lock): a panic in another thread must not
/// take the sidebar's scores down with it.
fn score_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, CachedScore>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, CachedScore>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// How many times [`cached_score`] has actually re-parsed a transcript, as
/// opposed to answering from the cache. Only the tests read it, but it is
/// counted unconditionally: an atomic increment on the recompute path costs
/// nothing next to the parse it is counting, and a `cfg(test)`-only counter
/// would measure a different code path than the one that ships.
static SCORE_RECOMPUTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// This session's current rot score, recomputed only when its transcript has
/// changed since the last call. Built for the dashboard sidebar: up to nine
/// panes polling about once a second, where a full parse per pane per second
/// would be far too expensive.
///
/// The steady state is a single `metadata` call: the resolved transcript path
/// is cached with the score, and an unchanged (mtime, len) answers straight
/// from memory. A changed transcript falls into the same incremental fold the
/// Stop hook uses ([`score_transcript_cached`]), which costs the appended
/// bytes rather than the session. Nothing here spawns, waits, or touches the
/// network.
///
/// `None` means *unknown*, never *healthy*: no transcript yet, a transcript
/// that cannot be read, an unresolvable config or agent. A renderer must show
/// that as `--`, since "healthy" and "unknown" are opposite things to tell an
/// operator. A session whose agent has not written its first line yet still
/// picks its score up once it does: the resolved path is stat-ed on every
/// poll, and re-resolved every [`RESOLVE_RETRY_POLLS`] polls while nothing is
/// there.
///
/// The dashboard sidebar that consumes this is a separate change in flight;
/// until it lands nothing in the binary calls it.
#[allow(dead_code)]
pub fn cached_score(state: &StateDir, repo: &Path, session_id: &str) -> Option<u32> {
    cached_score_with(state, repo, session_id, &env_from_process())
}

fn cached_score_with(
    state: &StateDir,
    repo: &Path,
    session_id: &str,
    env: EnvLookup<'_>,
) -> Option<u32> {
    let cached = {
        let cache = score_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.get(session_id).cloned()
    };

    if let Some(entry) = &cached {
        match stamp_of(&entry.transcript) {
            // The whole fast path: one stat, no config load, no parse.
            Some(stamp) => {
                if let Some((scored, score)) = &entry.scored
                    && *scored == stamp
                {
                    return Some(*score);
                }
            }
            // Nothing written there (yet, or any more). Keep answering
            // "unknown" off the path already resolved rather than resolving
            // it again on every frame.
            None => {
                let polls = entry.polls_since_resolve.saturating_add(1);
                if polls < RESOLVE_RETRY_POLLS {
                    let mut cache = score_cache().lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(live) = cache.get_mut(session_id) {
                        live.polls_since_resolve = polls;
                    }
                    return None;
                }
            }
        }
    }

    let cfg = CtxConfig::load(repo, env).ok()?;
    let adapter = adapters::select(cfg.agent.as_deref(), &[], &cfg).ok()?;
    let transcript = adapter.transcript_path(&SessionRef {
        id: SessionId::parse(session_id),
        cwd: repo.to_path_buf(),
    });
    // Stamped before the parse, so a line appended while it runs invalidates
    // this entry on the next poll instead of being missed forever.
    let scored = match stamp_of(&transcript) {
        Some(stamp) => score_with_checkpoint(state, &transcript, adapter.as_ref(), &cfg.score)
            .ok()
            .map(|score| {
                SCORE_RECOMPUTES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (stamp, score.score)
            }),
        None => None,
    };

    let mut cache = score_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.insert(
        session_id.to_string(),
        CachedScore {
            transcript,
            scored: scored.clone(),
            polls_since_resolve: 0,
        },
    );
    scored.map(|(_, score)| score)
}

pub fn run_with<W: Write>(
    args: &ScoreArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let score = score_transcript(&args.transcript, args.agent.as_deref(), repo, env)?;
    writeln!(w, "{}", serde_json::to_string(&score)?)?;
    Ok(0)
}

pub fn run<W: Write>(args: &ScoreArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn write_transcript(dir: &std::path::Path, turns: usize, marker: bool, tokens: u64) -> PathBuf {
        let mut text = String::new();
        for i in 0..turns {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str(
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":true}]}}\n",
            );
            let text_block = if marker || i < 2 {
                "[zirv] done"
            } else {
                "done"
            };
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{text_block}\"}}],\"usage\":{{\"input_tokens\":{tokens}}}}}}}\n"
            ));
        }
        let path = dir.join("t.jsonl");
        std::fs::write(&path, text).expect("write transcript");
        path
    }

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// A state dir of its own per test: the checkpoints are real files.
    fn state_env(dir: &Path) -> HashMap<String, String> {
        [(
            super::super::state::STATE_ENV.to_string(),
            dir.join("state").display().to_string(),
        )]
        .into()
    }

    /// Grows `transcript` towards `body` in `chunks` appends cut at line
    /// boundaries, scoring through the cached path after every one, and
    /// returns the last score. The final write is `body` byte for byte, so a
    /// transcript with no trailing newline stays one.
    fn replay(transcript: &Path, body: &str, chunks: usize, env: EnvLookup<'_>) -> Score {
        let repo = transcript.parent().unwrap_or(Path::new("."));
        let mut cuts: Vec<usize> = body.match_indices('\n').map(|(i, _)| i + 1).collect();
        if cuts.last() != Some(&body.len()) {
            cuts.push(body.len());
        }
        let step = cuts.len().div_ceil(chunks.max(1)).max(1);

        let mut score = None;
        let mut at_end = false;
        for cut in cuts
            .iter()
            .step_by(step)
            .chain(std::iter::once(&body.len()))
        {
            if at_end {
                break;
            }
            at_end = *cut == body.len();
            std::fs::write(transcript, &body[..*cut]).expect("write transcript");
            score = Some(
                score_transcript_cached(transcript, None, repo, env).expect("cached score runs"),
            );
        }
        score.expect("at least one pass")
    }

    /// The contract in one test: the recorded real session, fed in any number
    /// of appends, has to end on the byte-identical score one full parse
    /// produces from the same bytes.
    #[test]
    fn replaying_the_real_fixture_in_chunks_matches_a_full_parse() {
        let jsonl = std::fs::read_to_string(fixture_path("claude-real-session.jsonl"))
            .expect("fixture must be committed");

        for chunks in [1, 2, 7, 40, jsonl.lines().count()] {
            let dir = tempfile::tempdir().expect("tempdir");
            let env = state_env(dir.path());
            let transcript = dir.path().join("session.jsonl");

            let incremental = replay(&transcript, &jsonl, chunks, &|k| env.get(k).cloned());
            let full = score_transcript(&transcript, None, dir.path(), &|k| env.get(k).cloned())
                .expect("full score runs");
            assert_eq!(incremental, full, "the fixture fed in {chunks} chunks");
        }
    }

    /// The same equivalence for shapes the fixture happens not to contain: a
    /// rotting session, an empty file, and a transcript whose last line has no
    /// trailing newline.
    #[test]
    fn replaying_synthetic_transcripts_in_chunks_matches_a_full_parse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = state_env(dir.path());
        let rotting = std::fs::read_to_string(write_transcript(dir.path(), 14, false, 170_000))
            .expect("read");

        for (name, body) in [
            ("a rotting session", rotting.as_str()),
            ("an empty transcript", ""),
            (
                "no trailing newline",
                "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[zirv] ok\"}],\"usage\":{\"input_tokens\":9}}}",
            ),
        ] {
            for chunks in [1, 3, body.lines().count().max(1)] {
                let case = tempfile::tempdir().expect("tempdir");
                let env2 = state_env(case.path());
                let transcript = case.path().join("session.jsonl");

                let incremental = replay(&transcript, body, chunks, &|k| env2.get(k).cloned());
                let full =
                    score_transcript(&transcript, None, case.path(), &|k| env2.get(k).cloned())
                        .expect("full score runs");
                assert_eq!(incremental, full, "{name} in {chunks} chunks");
            }
        }
        drop(env);
    }

    /// A line still being written is scored on this pass but never committed,
    /// so the pass that sees it complete scores it exactly once.
    #[test]
    fn a_half_written_line_is_scored_without_being_committed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = state_env(dir.path());
        let lookup = |k: &str| env.get(k).cloned();
        let transcript = dir.path().join("session.jsonl");

        let complete = std::fs::read_to_string(write_transcript(dir.path(), 12, false, 170_000))
            .expect("read");
        let cut = complete.len() - 40;
        std::fs::write(&transcript, &complete[..cut]).expect("write a torn tail");
        let torn = score_transcript_cached(&transcript, None, dir.path(), &lookup).expect("scores");
        assert_eq!(
            torn,
            score_transcript(&transcript, None, dir.path(), &lookup).expect("full"),
            "a torn tail scores the same either way"
        );

        std::fs::write(&transcript, &complete).expect("finish the line");
        let finished =
            score_transcript_cached(&transcript, None, dir.path(), &lookup).expect("scores");
        assert_eq!(
            finished,
            score_transcript(&transcript, None, dir.path(), &lookup).expect("full"),
            "and the completed line is counted once, not twice"
        );
    }

    /// The performance claim: pass two advances the checkpoint by exactly the
    /// bytes that were appended, so turn N costs the turn and not the session.
    #[test]
    fn the_checkpoint_advances_by_only_the_appended_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = state_env(dir.path());
        let lookup = |k: &str| env.get(k).cloned();
        let transcript = dir.path().join("session.jsonl");

        let bulk =
            std::fs::read_to_string(write_transcript(dir.path(), 30, true, 120_000)).expect("read");
        std::fs::write(&transcript, &bulk).expect("write");
        score_transcript_cached(&transcript, None, dir.path(), &lookup).expect("scores");

        let state = StateDir::from_root(dir.path().join("state"));
        let path = checkpoint_path(&state, &transcript);
        let read_offset = |label: &str| -> u64 {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{label}: {e}"));
            serde_json::from_str::<serde_json::Value>(&text).expect("valid checkpoint")["offset"]
                .as_u64()
                .expect("offset")
        };
        assert_eq!(read_offset("first pass"), bulk.len() as u64);

        let turn = "{\"type\":\"user\",\"message\":{\"content\":\"more\"}}\n";
        std::fs::write(&transcript, format!("{bulk}{turn}")).expect("append one turn");
        score_transcript_cached(&transcript, None, dir.path(), &lookup).expect("scores");
        assert_eq!(
            read_offset("second pass") - bulk.len() as u64,
            turn.len() as u64,
            "the second pass folded in only the appended turn"
        );
    }

    /// Every way a checkpoint can stop describing the file it was written for.
    /// All of them have to land on the full-parse answer, silently.
    #[test]
    fn every_invalidation_path_falls_back_to_a_full_parse() {
        let corrupt = |path: &Path| std::fs::write(path, "{not json at all").expect("corrupt");
        let wrong_version = |path: &Path| {
            let text = std::fs::read_to_string(path).expect("read");
            let mut json: serde_json::Value = serde_json::from_str(&text).expect("json");
            json["version"] = serde_json::json!(CHECKPOINT_VERSION + 1);
            std::fs::write(path, json.to_string()).expect("write");
        };
        let wrong_transcript = |path: &Path| {
            let text = std::fs::read_to_string(path).expect("read");
            let mut json: serde_json::Value = serde_json::from_str(&text).expect("json");
            json["transcript"] = serde_json::json!("/somewhere/else/other-session.jsonl");
            std::fs::write(path, json.to_string()).expect("write");
        };
        let offset_past_the_end = |path: &Path| {
            let text = std::fs::read_to_string(path).expect("read");
            let mut json: serde_json::Value = serde_json::from_str(&text).expect("json");
            json["offset"] = serde_json::json!(u64::MAX);
            std::fs::write(path, json.to_string()).expect("write");
        };
        let deleted = |path: &Path| std::fs::remove_file(path).expect("remove");

        for (name, damage) in [
            ("corrupt", &corrupt as &dyn Fn(&Path)),
            ("a newer schema", &wrong_version),
            ("another session's", &wrong_transcript),
            ("an offset past the end", &offset_past_the_end),
            ("missing", &deleted),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let env = state_env(dir.path());
            let lookup = |k: &str| env.get(k).cloned();
            let transcript = dir.path().join("session.jsonl");
            let body = std::fs::read_to_string(write_transcript(dir.path(), 12, false, 170_000))
                .expect("read");
            std::fs::write(&transcript, &body).expect("write");

            score_transcript_cached(&transcript, None, dir.path(), &lookup).expect("first pass");
            let state = StateDir::from_root(dir.path().join("state"));
            damage(&checkpoint_path(&state, &transcript));

            std::fs::write(
                &transcript,
                format!("{body}{{\"type\":\"user\",\"message\":{{\"content\":\"go\"}}}}\n"),
            )
            .expect("append");
            assert_eq!(
                score_transcript_cached(&transcript, None, dir.path(), &lookup)
                    .expect("still scores"),
                score_transcript(&transcript, None, dir.path(), &lookup).expect("full"),
                "a {name} checkpoint must fall back to a full parse"
            );
        }
    }

    /// A transcript that shrank or was rewritten under a live checkpoint: the
    /// stored offset points into bytes that no longer mean anything.
    #[test]
    fn a_truncated_or_rewritten_transcript_is_rescored_from_scratch() {
        for rewrite in [true, false] {
            let dir = tempfile::tempdir().expect("tempdir");
            let env = state_env(dir.path());
            let lookup = |k: &str| env.get(k).cloned();
            let transcript = dir.path().join("session.jsonl");
            let long = std::fs::read_to_string(write_transcript(dir.path(), 14, false, 170_000))
                .expect("read");
            std::fs::write(&transcript, &long).expect("write");
            score_transcript_cached(&transcript, None, dir.path(), &lookup).expect("first pass");

            // Truncation, or a rewrite that is longer than what came before:
            // a post-compaction transcript can look like either.
            let replacement = if rewrite {
                let mut text =
                    std::fs::read_to_string(write_transcript(dir.path(), 20, true, 40_000))
                        .expect("read");
                text.push_str("{\"type\":\"system\",\"subtype\":\"compact_boundary\"}\n");
                text
            } else {
                long.lines().take(6).collect::<Vec<_>>().join("\n") + "\n"
            };
            std::fs::write(&transcript, &replacement).expect("replace");

            assert_eq!(
                score_transcript_cached(&transcript, None, dir.path(), &lookup)
                    .expect("still scores"),
                score_transcript(&transcript, None, dir.path(), &lookup).expect("full"),
                "rewrite={rewrite}"
            );
        }
    }

    /// Changing the scoring rules changes what the retained state should have
    /// kept, so the checkpoint written under the old ones must not be reused.
    #[test]
    fn a_config_change_rebuilds_instead_of_reusing_the_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut env = state_env(dir.path());
        let transcript = dir.path().join("session.jsonl");
        let body = std::fs::read_to_string(write_transcript(dir.path(), 14, false, 170_000))
            .expect("read");
        std::fs::write(&transcript, &body).expect("write");

        let first =
            score_transcript_cached(&transcript, None, dir.path(), &|k| env.get(k).cloned())
                .expect("first pass");
        assert_eq!(first.signals.marker_miss_rate, Some(1.0));

        env.insert("ZIRV_CTX_WINDOW".to_string(), "4".to_string());
        std::fs::write(
            &transcript,
            format!("{body}{{\"type\":\"user\",\"message\":{{\"content\":\"go\"}}}}\n"),
        )
        .expect("append");

        let lookup = |k: &str| env.get(k).cloned();
        assert_eq!(
            score_transcript_cached(&transcript, None, dir.path(), &lookup).expect("scores"),
            score_transcript(&transcript, None, dir.path(), &lookup).expect("full"),
            "a narrower window must be honoured, not read off stale state"
        );
    }

    /// An unbounded window keeps no state at all; it still has to score.
    #[test]
    fn an_unbounded_window_still_scores_correctly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut env = state_env(dir.path());
        env.insert("ZIRV_CTX_WINDOW".to_string(), "0".to_string());
        let lookup = |k: &str| env.get(k).cloned();
        let transcript = dir.path().join("session.jsonl");
        std::fs::write(
            &transcript,
            std::fs::read_to_string(write_transcript(dir.path(), 12, false, 170_000))
                .expect("read"),
        )
        .expect("write");

        assert_eq!(
            score_transcript_cached(&transcript, None, dir.path(), &lookup).expect("scores"),
            score_transcript(&transcript, None, dir.path(), &lookup).expect("full")
        );
    }

    #[test]
    fn the_checkpoint_file_is_private_to_its_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = state_env(dir.path());
        let transcript = dir.path().join("session.jsonl");
        std::fs::write(
            &transcript,
            std::fs::read_to_string(write_transcript(dir.path(), 4, true, 10_000)).expect("read"),
        )
        .expect("write");
        score_transcript_cached(&transcript, None, dir.path(), &|k| env.get(k).cloned())
            .expect("scores");

        let state = StateDir::from_root(dir.path().join("state"));
        let path = checkpoint_path(&state, &transcript);
        assert!(path.is_file(), "a checkpoint was written");
        assert!(
            std::fs::read_dir(state.scoring())
                .expect("read dir")
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")),
            "the staged copy is renamed into place, not left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "transcript state is nobody else's");
        }
    }

    #[test]
    fn a_missing_transcript_is_still_an_error_on_the_cached_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = state_env(dir.path());
        let err = score_transcript_cached(&dir.path().join("nope.jsonl"), None, dir.path(), &|k| {
            env.get(k).cloned()
        })
        .expect_err("must fail");
        assert!(err.to_string().contains("nope.jsonl"), "got {err}");
    }

    /// Where `ClaudeAdapter::transcript_path` computes this session's
    /// transcript under a test `HOME`: `~/.claude/projects/<repo slug>/`,
    /// which uses the same character rule as `state::repo_slug`.
    fn claude_transcript(home: &Path, repo: &Path, session: &str) -> PathBuf {
        let dir = home
            .join(".claude")
            .join("projects")
            .join(super::super::state::repo_slug(repo));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir.join(format!("{session}.jsonl"))
    }

    fn recomputes() -> u64 {
        SCORE_RECOMPUTES.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The dashboard polls this about once a second per pane, for up to nine
    /// panes. The contract is that an unchanged transcript costs no parse at
    /// all, and a changed one is picked up on the very next poll.
    #[test]
    fn the_cached_score_recomputes_only_when_the_transcript_changes() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(home.path().join("state"));
        let env: HashMap<String, String> = HashMap::new();
        let lookup = |k: &str| env.get(k).cloned();
        let session = "5c0d0001-1111-4222-8333-444444444444";

        assert_eq!(
            cached_score_with(&state, repo.path(), session, &lookup),
            None,
            "no transcript yet is unknown, and a renderer must show '--' rather than 0"
        );

        let transcript = claude_transcript(home.path(), repo.path(), session);
        let body = std::fs::read_to_string(write_transcript(repo.path(), 12, false, 170_000))
            .expect("read");
        std::fs::write(&transcript, &body).expect("write");

        let before = recomputes();
        let first = cached_score_with(&state, repo.path(), session, &lookup).expect("scores");
        assert_eq!(recomputes() - before, 1, "the first call has to parse");
        assert_eq!(
            first,
            score_transcript(&transcript, None, repo.path(), &lookup)
                .expect("full")
                .score,
            "and it must agree with a full parse"
        );

        for poll in 0..5 {
            assert_eq!(
                cached_score_with(&state, repo.path(), session, &lookup),
                Some(first),
                "poll {poll} of an unchanged transcript"
            );
        }
        assert_eq!(
            recomputes() - before,
            1,
            "an unchanged transcript must never be re-parsed"
        );

        std::fs::write(
            &transcript,
            format!("{body}{{\"type\":\"user\",\"message\":{{\"content\":\"go\"}}}}\n"),
        )
        .expect("append a turn");

        let after = cached_score_with(&state, repo.path(), session, &lookup).expect("scores");
        assert_eq!(
            recomputes() - before,
            2,
            "a changed transcript is picked up on the next poll"
        );
        assert_eq!(
            after,
            score_transcript(&transcript, None, repo.path(), &lookup)
                .expect("full")
                .score,
            "and still agrees with a full parse of the new bytes"
        );
    }

    /// A transcript that goes away is unknown again, not zero: the two mean
    /// opposite things to an operator reading the sidebar.
    #[test]
    fn a_transcript_that_disappears_reads_as_unknown_not_healthy() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(home.path().join("state"));
        let env: HashMap<String, String> = HashMap::new();
        let lookup = |k: &str| env.get(k).cloned();
        let session = "5c0d0002-1111-4222-8333-444444444444";

        let transcript = claude_transcript(home.path(), repo.path(), session);
        std::fs::write(
            &transcript,
            std::fs::read_to_string(write_transcript(repo.path(), 4, true, 10_000)).expect("read"),
        )
        .expect("write");
        assert!(cached_score_with(&state, repo.path(), session, &lookup).is_some());

        std::fs::remove_file(&transcript).expect("remove");
        let before = recomputes();
        for poll in 0..RESOLVE_RETRY_POLLS - 1 {
            assert_eq!(
                cached_score_with(&state, repo.path(), session, &lookup),
                None,
                "poll {poll}: a stale score must not outlive the transcript it was read from"
            );
        }
        assert_eq!(
            recomputes() - before,
            0,
            "and a missing transcript must not put a directory walk on every frame"
        );

        // The transcript comes back (a session relaunched into the same id):
        // the very next poll after the retry window picks it up.
        std::fs::write(
            &transcript,
            std::fs::read_to_string(write_transcript(repo.path(), 12, false, 170_000))
                .expect("read"),
        )
        .expect("rewrite");
        assert!(
            cached_score_with(&state, repo.path(), session, &lookup).is_some(),
            "a transcript that reappears is scored again"
        );
    }

    #[test]
    fn prints_one_line_of_json_with_the_documented_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = write_transcript(dir.path(), 12, false, 170_000);
        let args = ScoreArgs {
            transcript,
            agent: None,
        };

        let mut out = Vec::new();
        let code = run_with(&args, &mut out, dir.path(), &|_| None).expect("score runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(text.lines().count(), 1, "exactly one JSON line");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
        assert!(parsed["score"].is_u64());
        assert_eq!(parsed["verdict"], "restart");
        assert_eq!(parsed["context_tokens"], 170_000);
        assert_eq!(parsed["signals"]["turns"], 12);
        assert_eq!(parsed["signals"]["tool_failure_rate"], 1.0);
        assert_eq!(parsed["signals"]["marker_miss_rate"], 1.0);
    }

    #[test]
    fn an_inactive_marker_signal_serializes_as_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = write_transcript(dir.path(), 12, true, 120_000);
        let args = ScoreArgs {
            transcript,
            agent: None,
        };

        let mut out = Vec::new();
        run_with(&args, &mut out, dir.path(), &|_| None).expect("score runs");
        let parsed: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).expect("utf8").trim()).expect("json");
        assert_eq!(parsed["signals"]["marker_miss_rate"], 0.0);
    }

    #[test]
    fn repo_config_changes_the_verdict() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            dir.path().join(".zirv/ctx.toml"),
            "[score]\ntoken_floor = 500000\ntoken_ceiling = 900000\n",
        )
        .expect("write");
        let transcript = write_transcript(dir.path(), 12, false, 170_000);
        let args = ScoreArgs {
            transcript,
            agent: None,
        };

        let mut out = Vec::new();
        run_with(&args, &mut out, dir.path(), &|_| None).expect("score runs");
        let parsed: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).expect("utf8").trim()).expect("json");
        assert_eq!(
            parsed["verdict"], "healthy",
            "the raised floor gates everything"
        );
    }

    #[test]
    fn a_missing_transcript_is_an_error_not_a_healthy_verdict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = ScoreArgs {
            transcript: dir.path().join("nope.jsonl"),
            agent: None,
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, dir.path(), &|_| None).expect_err("must fail");
        assert!(err.to_string().contains("nope.jsonl"), "got {err}");
    }

    #[test]
    fn env_overrides_reach_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = write_transcript(dir.path(), 12, false, 170_000);
        let args = ScoreArgs {
            transcript,
            agent: None,
        };
        let env: HashMap<String, String> =
            [("ZIRV_CTX_MARKER".to_string(), "[other]".to_string())].into();

        let mut out = Vec::new();
        run_with(&args, &mut out, dir.path(), &|k| env.get(k).cloned()).expect("score runs");
        let parsed: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).expect("utf8").trim()).expect("json");
        assert!(
            parsed["signals"]["marker_miss_rate"].is_null(),
            "a marker that never appears deactivates the signal"
        );
    }
}
