//! Inter-agent mailbox: agent sessions leave markdown notes for other
//! sessions to read. Mirrors `handoff.rs`'s storage idioms (zero-padded
//! seconds prefix, `state::write_private`, tolerant markdown parsing) but
//! consumed messages are moved into a `read/` subdirectory rather than
//! pruned or deleted, since a mail message is meant to be read exactly once
//! by whichever session gets to it first.

use std::path::{Path, PathBuf};

use super::CtxResult;
use super::config::CtxConfig;
use super::state::{StateDir, now_secs};

/// One mail message: a free-form markdown note plus who sent it, who it is
/// addressed to, and when.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub from_session: String,
    pub from_agent: String,
    /// Recipient agent name, or `"any"` for every agent. `"any"` is the
    /// default produced by `parse_markdown` when the header is absent.
    pub to: String,
    /// Unix seconds the message was authored.
    pub sent: u64,
    pub body: String,
}

impl Message {
    /// Renders the `## Message` header block (From-session, From-agent, To,
    /// Sent as list items) followed by the free markdown body.
    pub fn to_markdown(&self) -> String {
        format!(
            "## Message\n- From-session: {}\n- From-agent: {}\n- To: {}\n- Sent: {}\n\n{}\n",
            self.from_session, self.from_agent, self.to, self.sent, self.body
        )
    }
}

/// Same bullet styles `handoff::strip_bullet` accepts. Duplicated locally
/// (rather than made `pub(crate)` in handoff.rs) to keep this file's edits
/// isolated from a file another task is actively working in.
fn strip_bullet(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && trimmed[digits.len()..].starts_with(". ") {
        return Some(trimmed[digits.len() + 2..].trim().to_string());
    }
    None
}

/// Parses a `## Message` header block and body with the same tolerance as
/// `handoff::parse_markdown`: unknown headers and unknown sections are
/// skipped rather than treated as an error.
pub fn parse_markdown(md: &str) -> Message {
    let mut msg = Message {
        from_session: String::new(),
        from_agent: String::new(),
        to: "any".to_string(),
        sent: 0,
        body: String::new(),
    };
    let mut in_message = false;
    let mut in_header = false;
    let mut body_lines: Vec<&str> = Vec::new();

    for line in md.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("## ") {
            in_message = rest.trim().eq_ignore_ascii_case("Message");
            in_header = in_message;
            continue;
        }
        if !in_message {
            continue;
        }
        if in_header {
            if trimmed.is_empty() {
                continue;
            }
            if let Some(bullet) = strip_bullet(line) {
                if let Some((key, value)) = bullet.split_once(':') {
                    match key.trim().to_ascii_lowercase().as_str() {
                        "from-session" => msg.from_session = value.trim().to_string(),
                        "from-agent" => msg.from_agent = value.trim().to_string(),
                        "to" => msg.to = value.trim().to_string(),
                        "sent" => msg.sent = value.trim().parse().unwrap_or(0),
                        // Unknown header inside the block: skipped, not an error.
                        _ => {}
                    }
                    continue;
                }
            }
            // First non-bullet, non-blank line ends the header block.
            in_header = false;
        }
        body_lines.push(line);
    }

    msg.body = body_lines.join("\n").trim().to_string();
    msg
}

/// Truncates `s` to at most `max_bytes` bytes without splitting a UTF-8
/// character.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Writes `msg` under `<state>/mail/<repo_slug>/`, truncating an oversized
/// body (never failing the store) and pruning the directory down to the
/// newest `cfg.mail.keep` unread messages.
pub fn store(
    state: &StateDir,
    repo_slug: &str,
    msg: &Message,
    cfg: &CtxConfig,
) -> CtxResult<PathBuf> {
    let dir = state.mail().join(repo_slug);
    super::state::create_private_dir_all(&dir)?;

    let mut msg = msg.clone();
    let cap = cfg.mail.max_message_bytes;
    if msg.body.len() > cap {
        const MARKER: &str = "\n[truncated]";
        let keep = cap.saturating_sub(MARKER.len());
        let mut truncated = truncate_at_char_boundary(&msg.body, keep).to_string();
        truncated.push_str(MARKER);
        msg.body = truncated;
    }

    let short: String = msg
        .from_session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let path = dir.join(format!("{:010}-{}.md", now_secs(), short));
    super::state::write_private(&path, &msg.to_markdown())?;

    super::state::prune_to_newest(&dir, cfg.mail.keep);
    Ok(path)
}

/// Lists unread messages for `repo_slug`, oldest first, visible to
/// `for_agent` (or every message when `for_agent` is `None`). Individual
/// files that cannot be read or parsed are skipped rather than failing the
/// whole listing.
pub fn list(
    state: &StateDir,
    repo_slug: &str,
    for_agent: Option<&str>,
) -> CtxResult<Vec<(PathBuf, Message)>> {
    let dir = state.mail().join(repo_slug);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    // The zero-padded seconds prefix in each file name sorts lexicographic
    // order into chronological order, the same convention `state::now_secs`
    // documents for handoffs and log lines. `read/`, a directory rather than
    // a `.md` file, is excluded by the `is_file` filter below.
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let msg = parse_markdown(&text);
        let visible = match for_agent {
            None => true,
            Some(agent) => msg.to.eq_ignore_ascii_case("any") || msg.to.eq_ignore_ascii_case(agent),
        };
        if visible {
            out.push((path, msg));
        }
    }
    Ok(out)
}

/// Moves a message into `read/`, creating the subdirectory as needed.
/// Consumed messages are never deleted.
pub fn consume(state: &StateDir, repo_slug: &str, path: &Path) -> CtxResult<()> {
    let read_dir = state.mail().join(repo_slug).join("read");
    super::state::create_private_dir_all(&read_dir)?;
    let file_name = path
        .file_name()
        .ok_or("mail message path has no file name")?;
    std::fs::rename(path, read_dir.join(file_name))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(from_session: &str, sent: u64) -> Message {
        Message {
            from_session: from_session.to_string(),
            from_agent: "claude".to_string(),
            to: "any".to_string(),
            sent,
            body: "Heads up: the webhook route moved to /v2/webhook.".to_string(),
        }
    }

    #[test]
    fn a_message_is_stored_under_the_repo_slug_with_a_sortable_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let path = store(
            &state,
            "-work-my-repo",
            &sample("11111111-2222", 1_700_000_000),
            &cfg,
        )
        .expect("store");

        assert!(path.starts_with(state.mail().join("-work-my-repo")));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));
        let name = path.file_name().and_then(|n| n.to_str()).expect("utf8");
        let digits = &name[..10];
        assert!(
            digits.chars().all(|c| c.is_ascii_digit()),
            "zero-padded seconds prefix sorts lexicographically: {name}"
        );
        assert!(
            name.contains("11111111"),
            "from_session short id in the name: {name}"
        );

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("## Message"));
    }

    #[test]
    fn messages_are_listed_oldest_first_and_never_leak_across_repos() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));

        let dir_a = state.mail().join("-work-a");
        std::fs::create_dir_all(&dir_a).expect("mkdir");
        std::fs::write(
            dir_a.join("1700000000-aaaa.md"),
            sample("aaaa", 1_700_000_000).to_markdown(),
        )
        .expect("write");
        std::fs::write(
            dir_a.join("1700000900-bbbb.md"),
            sample("bbbb", 1_700_000_900).to_markdown(),
        )
        .expect("write");

        let dir_b = state.mail().join("-work-b");
        std::fs::create_dir_all(&dir_b).expect("mkdir");
        std::fs::write(
            dir_b.join("1700000500-cccc.md"),
            sample("cccc", 1_700_000_500).to_markdown(),
        )
        .expect("write");

        let listed = list(&state, "-work-a", None).expect("list");
        assert_eq!(listed.len(), 2, "only repo a's messages, none from b");
        assert!(listed[0].0.ends_with("1700000000-aaaa.md"));
        assert!(listed[1].0.ends_with("1700000900-bbbb.md"));
        assert_eq!(listed[0].1.from_session, "aaaa");
        assert_eq!(listed[1].1.from_session, "bbbb");
    }

    #[test]
    fn consuming_a_message_moves_it_to_the_read_subdirectory_rather_than_deleting_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let path = store(&state, "-work-repo", &sample("s1", 1), &cfg).expect("store");

        consume(&state, "-work-repo", &path).expect("consume");

        assert!(!path.exists(), "the original path is gone");
        let moved = state
            .mail()
            .join("-work-repo")
            .join("read")
            .join(path.file_name().expect("file name"));
        assert!(moved.exists(), "moved into read/, not deleted");
        assert!(
            list(&state, "-work-repo", None).expect("list").is_empty(),
            "consumed messages are excluded from list"
        );
    }

    #[test]
    fn a_message_longer_than_the_cap_is_truncated_and_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.mail.max_message_bytes = 50;

        let mut msg = sample("s1", 1);
        msg.body = "x".repeat(500);

        let path =
            store(&state, "-work-repo", &msg, &cfg).expect("store must not fail on oversize");
        let stored = parse_markdown(&std::fs::read_to_string(&path).expect("read"));
        assert!(
            stored.body.len() <= 50,
            "body respects the cap: {} bytes",
            stored.body.len()
        );
        assert!(
            stored.body.ends_with("[truncated]"),
            "says it was truncated: {}",
            stored.body
        );
    }

    #[test]
    fn the_header_block_round_trips_sender_recipient_and_timestamp() {
        let msg = Message {
            from_session: "11111111-2222".to_string(),
            from_agent: "claude".to_string(),
            to: "codex".to_string(),
            sent: 1_700_000_000,
            body: "Heads up: schema migration landed on main.".to_string(),
        };
        let parsed = parse_markdown(&msg.to_markdown());
        assert_eq!(parsed, msg);
    }

    #[test]
    fn an_unknown_header_or_section_is_skipped_rather_than_failing_the_read() {
        let md = "## Message\n\
- From-session: 11111111\n\
- From-agent: claude\n\
- Priority: urgent\n\
- To: any\n\
- Sent: 1700000000\n\
\n\
Body text here.\n\
\n\
## Footer\n\
This should not appear in the body.\n";

        let msg = parse_markdown(md);
        assert_eq!(msg.from_session, "11111111");
        assert_eq!(msg.from_agent, "claude");
        assert_eq!(msg.to, "any");
        assert_eq!(msg.sent, 1_700_000_000);
        assert_eq!(msg.body, "Body text here.");
    }

    #[test]
    fn a_message_addressed_to_one_agent_is_not_listed_for_another() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let dir = state.mail().join("-work-repo");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mut for_claude = sample("s1", 1_700_000_000);
        for_claude.to = "claude".to_string();
        std::fs::write(dir.join("1700000000-s1.md"), for_claude.to_markdown()).expect("write");

        let mut for_any = sample("s2", 1_700_000_100);
        for_any.to = "any".to_string();
        std::fs::write(dir.join("1700000100-s2.md"), for_any.to_markdown()).expect("write");

        let for_codex = list(&state, "-work-repo", Some("codex")).expect("list");
        assert_eq!(
            for_codex.len(),
            1,
            "only the 'any' message is visible to codex"
        );
        assert_eq!(for_codex[0].1.from_session, "s2");

        let for_claude_listing = list(&state, "-work-repo", Some("claude")).expect("list");
        assert_eq!(
            for_claude_listing.len(),
            2,
            "claude sees both its own message and the 'any' one"
        );
    }

    #[test]
    fn the_mail_directory_is_pruned_to_the_newest_keep_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.mail.keep = 3;

        let dir = state.mail().join("-work-repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let base = std::time::SystemTime::now();
        for index in 0..5u32 {
            let path = dir.join(format!("170000000{index}-msg.md"));
            std::fs::write(&path, sample("s", 1).to_markdown()).expect("write");
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open")
                .set_modified(base + std::time::Duration::from_secs(index as u64))
                .expect("set_modified");
        }

        store(&state, "-work-repo", &sample("new", 1_700_000_999), &cfg).expect("store");

        let remaining = list(&state, "-work-repo", None).expect("list");
        assert_eq!(
            remaining.len(),
            cfg.mail.keep,
            "pruned down to the newest keep entries"
        );
    }
}
