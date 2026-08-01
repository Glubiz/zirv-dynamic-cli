use std::io::Write;
use std::path::{Path, PathBuf};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::rot::{self, Score};
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

/// Shared by `hook`, `exec`, `loop` and `wrap`: read a transcript, parse it with
/// the selected adapter, score it.
pub fn score_transcript(
    transcript: &Path,
    agent: Option<&str>,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<Score> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(
        agent.or(cfg.agent.as_deref()),
        &[],
        cfg.agent_bin.as_deref(),
    )?;
    let jsonl = std::fs::read_to_string(transcript)
        .map_err(|e| format!("{}: {e}", transcript.display()))?;
    let events = adapter.parse_events(&jsonl);
    Ok(rot::score_events(
        &events,
        adapter.capabilities(),
        &cfg.score,
    ))
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
