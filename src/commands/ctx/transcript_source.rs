//! Issue #382: materializes newly seen rows from a harness's own JSON
//! snapshot file or SQLite database into a zirv-owned "shadow" JSONL file, so
//! the unchanged, JSONL-based rot engine (`supervise::Watcher`,
//! `score::IncrementalScorer`, every `AgentAdapter::parse_events`) can score
//! a harness whose native transcript is not naturally line-local JSONL,
//! without teaching any of that machinery about JSON arrays or SQL.
//!
//! No trait method is added for this: an adapter for such a harness calls
//! [`ShadowTranscript::sync_json_array`] or [`ShadowTranscript::sync_sqlite`]
//! from inside its own `AgentAdapter::transcript_path` and returns the
//! resulting path instead of its harness's native file -- see that trait
//! method's own doc comment for the contract. `CodexAdapter::transcript_path`
//! already performs I/O inside the same call (scanning `~/.codex/sessions`
//! and pinning a resolved rollout under `StateDir::rollouts()`), so this is
//! existing precedent, not a new shape.
//!
//! Issue #382 is the foundation for wave-1 adapters (Gemini, OpenCode, Pi:
//! issues #384-#386) that do not exist yet -- nothing in the binary calls
//! into this module outside its own tests until one of those lands.
//! `#![allow(dead_code)]` covers it until then, the same reasoning
//! `dash::pane`'s own module doc comment already documents: a real,
//! fully-tested API with no in-tree caller yet is not the same thing as code
//! that should be deleted.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};

use super::CtxResult;
use super::event::SessionRef;
use super::sessions;
use super::state::{self, StateDir};

/// How long a session's shadow files are kept once nothing has synced them --
/// the same 30-day horizon `log::SAFETY_DECISION_RETENTION_DAYS` uses for the
/// safety-decision log, chosen so a shadow outlives any realistic
/// resumed/handed-over session without growing forever on a machine that has
/// run zirv for months. Unlike that log's day-bucketed files (one file per
/// UTC day, prunable by parsing the filename), a shadow directory holds one
/// file per SESSION -- there is no day to parse out of a short session id --
/// so pruning here reads each file's own mtime instead, the same signal
/// [`state::prune_to_newest`] reads, just compared against an age cutoff
/// rather than a keep-count.
const SHADOW_RETENTION_DAYS: u64 = 30;

/// Bytes comfortably larger than any single realistic JSONL row (one chat
/// message, or one row of a handful of SQLite columns) that [`tail_window`]
/// reads from a shadow's tail before falling back to a full read. Bounds how
/// much of a long-lived shadow gets read just to find its last line or two,
/// which is all [`ShadowTranscript::shadow_row_count`] and
/// [`ShadowTranscript::shadow_last_rowid`] ever need.
///
/// [`tail_window`]: ShadowTranscript::tail_window
const TAIL_READ_BYTES: u64 = 64 * 1024;

/// The last Codex assistant message, never tool arguments/results or commentary.
/// Rollouts may end without a `task_complete` event when the context is exhausted.
pub fn codex_final_assistant_message(jsonl: &str) -> Option<String> {
    for line in jsonl.lines().rev() {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = &row["payload"];
        if row["type"] == "response_item"
            && payload["type"] == "message"
            && payload["role"] == "user"
        {
            return None;
        }
        if row["type"] != "response_item"
            || payload["type"] != "message"
            || payload["role"] != "assistant"
            || payload["phase"] == "commentary"
        {
            continue;
        }
        let Some(content) = payload["content"].as_array() else {
            continue;
        };
        let text = content
            .iter()
            .filter(|item| item["type"] == "output_text")
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

/// One session's materialized shadow transcript:
/// `<state>/shadow/<short>.jsonl` holds the JSONL rows the rot engine reads.
/// A sibling of [`StateDir::rollouts`], with the same short-id derivation
/// ([`sessions::short_id`]).
///
/// There is deliberately no separate cursor file recording how much of a
/// source has already been synced: an earlier revision kept one, and a
/// crash between appending rows to the shadow and writing the cursor's new
/// value left the cursor behind what the shadow already held, so the next
/// sync trusted the stale cursor and replayed rows the shadow already had,
/// duplicating them. The shadow file is now the only record of its own
/// position -- [`Self::shadow_row_count`] (for [`Self::sync_json_array`])
/// and [`Self::shadow_last_rowid`] (for [`Self::sync_sqlite`]) read it
/// directly off the shadow's own bytes, so there is nothing left that can
/// fall out of sync with it.
pub struct ShadowTranscript {
    jsonl: PathBuf,
}

impl ShadowTranscript {
    /// Ensures `<state>/shadow` exists with the same private-directory
    /// discipline every other state-dir subdirectory gets
    /// ([`state::create_private_dir_all`]), prunes shadow files idle for
    /// more than [`SHADOW_RETENTION_DAYS`], then names this session's file
    /// inside it. Both steps are best-effort, matching every other
    /// lazy-create call site in this module (e.g.
    /// `adapters::codex::CodexAdapter::pinned_rollout`): a directory that
    /// cannot be created or pruned is not a reason a session should fail to
    /// resolve a transcript path. A later `sync_*` call against an
    /// unwritable directory fails softly in its own way (see those methods'
    /// doc comments) or surfaces a real I/O error, exactly as if this
    /// constructor had never tried.
    pub fn for_session(state: &StateDir, session: &SessionRef) -> Self {
        let dir = state.shadow();
        if state::create_private_dir_all(&dir).is_ok() {
            prune_idle_shadow_files(&dir, SHADOW_RETENTION_DAYS);
        }
        let short = sessions::short_id(session.id.as_str());
        Self {
            jsonl: dir.join(format!("{short}.jsonl")),
        }
    }

    /// The shadow JSONL path -- what an adapter's `transcript_path` returns
    /// after syncing.
    pub fn path(&self) -> &Path {
        &self.jsonl
    }

    /// Atomically empties the shadow JSONL. `state::write_private`'s
    /// temp-sibling-then-rename gives `supervise::Watcher::read_appended` an
    /// unambiguous restart signal (a new mtime at a shorter, usually zero,
    /// length) rather than the zero-length truncation window a plain
    /// truncate-in-place would leave a concurrent reader to observe.
    fn reset(&self) -> CtxResult<()> {
        state::create_private_dir_all(self.dir())?;
        state::write_private(&self.jsonl, "")?;
        Ok(())
    }

    fn dir(&self) -> &Path {
        // `for_session` always names the file as a direct child of the
        // shadow directory.
        self.jsonl.parent().unwrap_or(Path::new("."))
    }

    /// Reads `self.jsonl` from `start` to EOF.
    fn read_tail(&self, start: u64) -> CtxResult<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&self.jsonl)?;
        let len = file.metadata()?.len();
        let start = start.min(len);
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; (len - start) as usize];
        file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// A tail window of the shadow guaranteed to hold at least its last two
    /// complete lines, or the whole file, whichever is shorter -- the shared
    /// read behind both [`Self::repair_trailing_partial_line`] and
    /// [`Self::shadow_last_rowid`]. Starts with [`TAIL_READ_BYTES`] and only
    /// re-reads the whole file when that window holds fewer than two
    /// newlines while stopping short of the file's start, which only a
    /// pathologically large single line ever triggers. Returns the absolute
    /// file offset the window starts at, alongside its bytes; `(0, [])` for
    /// a missing or empty shadow.
    fn tail_window(&self) -> CtxResult<(u64, Vec<u8>)> {
        let len = match std::fs::metadata(&self.jsonl) {
            Ok(meta) => meta.len(),
            Err(_) => return Ok((0, Vec::new())),
        };
        if len == 0 {
            return Ok((0, Vec::new()));
        }
        let start = len.saturating_sub(TAIL_READ_BYTES);
        let window = self.read_tail(start)?;
        if start > 0 && window.iter().filter(|&&b| b == b'\n').count() < 2 {
            return Ok((0, self.read_tail(0)?));
        }
        Ok((start, window))
    }

    /// Drops a trailing partial line -- a write that stopped mid-row, e.g. a
    /// crash between two [`Self::append_rows`] calls -- and persists the
    /// truncation immediately. Both [`Self::shadow_row_count`] and
    /// [`Self::shadow_last_rowid`] call this first: with no cursor file to
    /// consult any more, the shadow's own bytes are the only record of a
    /// resume position, and a half-written line must never be read as if it
    /// had landed. A no-op when the shadow is missing, empty, or already
    /// ends in a complete line.
    fn repair_trailing_partial_line(&self) -> CtxResult<()> {
        let (start, window) = self.tail_window()?;
        if window.is_empty() || window.last() == Some(&b'\n') {
            return Ok(());
        }
        match window.iter().rposition(|&b| b == b'\n') {
            Some(pos) => {
                let keep_len = start + pos as u64 + 1;
                let file = std::fs::OpenOptions::new().write(true).open(&self.jsonl)?;
                file.set_len(keep_len)?;
            }
            None => {
                // `tail_window` only ever returns fewer than two newlines
                // while `start == 0` (see its own doc comment), so an empty
                // `rposition` here means the whole file is one partial
                // line/blob -- nothing complete survives.
                state::create_private_dir_all(self.dir())?;
                state::write_private(&self.jsonl, "")?;
            }
        }
        Ok(())
    }

    /// The shadow's own row count, once [`Self::repair_trailing_partial_line`]
    /// has dropped any trailing partial line -- the cursor
    /// [`Self::sync_json_array`] resumes from, read directly off the file
    /// instead of a separate record. Reads the whole shadow: unlike
    /// [`Self::shadow_last_rowid`], a total count cannot be answered from a
    /// tail window alone, and a JSON-array-sourced shadow is bounded by the
    /// harness's own snapshot size rather than growing without limit.
    fn shadow_row_count(&self) -> CtxResult<usize> {
        self.repair_trailing_partial_line()?;
        let Ok(bytes) = std::fs::read(&self.jsonl) else {
            return Ok(0);
        };
        Ok(bytes.iter().filter(|&&b| b == b'\n').count())
    }

    /// The `rowid` of the shadow's last complete line, once
    /// [`Self::repair_trailing_partial_line`] has dropped any trailing
    /// partial one -- the cursor [`Self::sync_sqlite`] resumes from, read
    /// directly off the file instead of a separate record. `0` when the
    /// shadow is empty or its last complete line is not a JSON object with
    /// an integer `rowid` field -- both read as "nothing to resume from",
    /// the same safe-restart posture [`Self::shadow_row_count`] gives an
    /// unreadable shadow.
    fn shadow_last_rowid(&self) -> CtxResult<i64> {
        self.repair_trailing_partial_line()?;
        let (_, window) = self.tail_window()?;
        if window.is_empty() {
            return Ok(0);
        }
        let end = window.len() - 1; // drop the trailing '\n'
        let line_start = window[..end]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |p| p + 1);
        let last_line = &window[line_start..end];
        Ok(serde_json::from_slice::<serde_json::Value>(last_line)
            .ok()
            .and_then(|value| value.get("rowid").and_then(serde_json::Value::as_i64))
            .unwrap_or(0))
    }

    /// Appends `rows` to the shadow JSONL, one compact JSON line per row,
    /// each written with a single `write_all` call so a reader watching the
    /// file's length never observes one row split across two writes.
    /// `state::open_private_append` gives the file the same 0600/append-mode
    /// discipline every other private state file gets.
    fn append_rows(&self, rows: &[serde_json::Value]) -> CtxResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        state::create_private_dir_all(self.dir())?;
        let mut file = state::open_private_append(&self.jsonl)?;
        for row in rows {
            let mut line = serde_json::to_string(row)?;
            line.push('\n');
            file.write_all(line.as_bytes())?;
        }
        Ok(())
    }

    /// Syncs a harness's JSON-array snapshot (a whole file rewritten on
    /// every turn, e.g. a chat log persisted as one JSON document) into the
    /// shadow JSONL.
    ///
    /// `extract` receives the parsed snapshot and returns its message rows,
    /// in the harness's own order; each row becomes one shadow line. The
    /// resume position is [`Self::shadow_row_count`] -- how many complete
    /// rows the shadow itself already holds, not a separate cursor record
    /// (see [`ShadowTranscript`]'s own doc comment for why). A snapshot that
    /// now has FEWER rows than that count -- a new session reusing the same
    /// file, or a genuine truncation -- rewrites the shadow from zero, which
    /// `Watcher::read_appended` already reads as a restart (see its own doc
    /// comment: same-or-shorter length is never treated as an append).
    ///
    /// A missing or unparseable source file leaves the shadow untouched and
    /// simply returns its path: a harness's snapshot being absent or
    /// mid-write is an ordinary, transient condition (the next poll heals
    /// it), never a reason to fail the caller and take a supervised session
    /// down over a transcript-parsing hiccup.
    pub fn sync_json_array(
        &self,
        source: &Path,
        extract: &dyn Fn(&serde_json::Value) -> Vec<serde_json::Value>,
    ) -> CtxResult<PathBuf> {
        let Ok(text) = std::fs::read_to_string(source) else {
            return Ok(self.jsonl.clone());
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Ok(self.jsonl.clone());
        };
        let rows = extract(&value);

        let written = self.shadow_row_count()?;
        if rows.len() < written {
            self.reset()?;
            self.append_rows(&rows)?;
        } else if rows.len() > written {
            self.append_rows(&rows[written..])?;
        }
        Ok(self.jsonl.clone())
    }

    /// Syncs rows past the cursor from a harness's SQLite transcript into
    /// the shadow JSONL.
    ///
    /// Opens the database with a plain `SQLITE_OPEN_READ_ONLY` connection
    /// (see [`open_for_sync`] for the fallback this prefers over). In WAL
    /// mode -- what a live harness transcript actually uses -- readers never
    /// block a writer and a writer never blocks them, which is the entire
    /// point of WAL, so this needs no `?immutable=1` promise to stay
    /// non-blocking. Unlike `?immutable=1`, a plain read-only open DOES
    /// attach the `-wal` file, so a row a writer has committed but not yet
    /// checkpointed into the main database file is visible immediately
    /// rather than lagging by up to a full checkpoint interval -- the
    /// staleness an `immutable=1`-only open would otherwise impose on every
    /// WAL-writing harness (OpenCode among them).
    ///
    /// `query` must select `rowid` as its first column, then the named
    /// columns to carry into the shadow line, and must contain exactly one
    /// `?1` placeholder bound to the last-seen rowid, e.g. `SELECT rowid,
    /// id, role, data, time_created FROM message WHERE rowid > ?1 ORDER BY
    /// rowid`. Each matching row becomes one compact JSON object keyed by
    /// column name (integers and reals as JSON numbers, text as a JSON
    /// string, `NULL` as JSON `null`) plus a `"rowid"` key holding the first
    /// column's value. Blob columns are skipped entirely (no key is
    /// written) rather than base64-encoded: a transcript database's blob
    /// columns are not verified to hold anything the rot engine's line-local
    /// text parsing would ever read, and skipping avoids pulling in a
    /// base64 dependency for bytes nothing downstream consumes. The resume
    /// position is [`Self::shadow_last_rowid`] -- the `rowid` field of the
    /// shadow's own last complete line, not a separate cursor record (see
    /// [`ShadowTranscript`]'s own doc comment for why).
    ///
    /// A missing database file, a database that fails to open under those
    /// flags, or a query that fails to prepare or execute, all leave the
    /// shadow untouched and return its path -- the same "never take a
    /// session down over a transcript read" posture `sync_json_array`
    /// documents above, extended to cover a harness's database being absent,
    /// locked in a way this read cannot tolerate, or simply not yet
    /// created.
    pub fn sync_sqlite(&self, db: &Path, query: &str) -> CtxResult<PathBuf> {
        if !db.is_file() {
            return Ok(self.jsonl.clone());
        }

        let cursor = self.shadow_last_rowid()?;

        let Some(conn) = open_for_sync(db) else {
            return Ok(self.jsonl.clone());
        };
        let Ok(mut stmt) = conn.prepare(query) else {
            return Ok(self.jsonl.clone());
        };
        let column_names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
        let Ok(mut rows) = stmt.query(rusqlite::params![cursor]) else {
            return Ok(self.jsonl.clone());
        };

        let mut new_rows = Vec::new();
        // A fault partway through iteration (e.g. a concurrent checkpoint)
        // simply stops the scan here -- whatever was already read is kept,
        // never discarded, and the next poll picks up past the last row
        // actually appended (`shadow_last_rowid` re-derives the cursor from
        // it on the next call).
        while let Some(row) = rows.next().ok().flatten() {
            let Ok(rowid) = row.get::<_, i64>(0) else {
                continue;
            };
            let mut obj = serde_json::Map::new();
            obj.insert("rowid".to_string(), serde_json::Value::from(rowid));
            for (index, name) in column_names.iter().enumerate().skip(1) {
                if let Ok(Some(value)) = column_value_json(row, index) {
                    obj.insert(name.clone(), value);
                }
            }
            new_rows.push(serde_json::Value::Object(obj));
        }

        self.append_rows(&new_rows)?;
        Ok(self.jsonl.clone())
    }
}

/// Opens `db` for [`ShadowTranscript::sync_sqlite`], preferring a plain
/// read-only connection over `?immutable=1`.
///
/// A plain `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` open (no URI, no
/// `immutable`) is an ordinary WAL reader: in WAL mode readers never block a
/// writer and a writer never blocks them, so nothing here needs the
/// `immutable` promise to stay non-blocking, and this form DOES attach the
/// `-wal` file, so a row committed but not yet checkpointed into the main
/// database is visible right away.
///
/// `?immutable=1` is kept only as a fallback for the one thing a plain
/// read-only open cannot itself do: even a read-only WAL connection still
/// needs to create/open the `-shm` (shared-memory wal-index) file next to
/// the database on first access, and that fails with `SQLITE_READONLY` or
/// `SQLITE_CANTOPEN` when the containing directory itself is not writable
/// (e.g. a transcript database shipped on read-only media). `immutable=1`
/// tells SQLite the file will never change and skips the `-shm`/`-wal`
/// machinery entirely, trading a possibly-stale read (a commit made after
/// this handle opened may not be visible until the next poll) for the
/// ability to read at all -- acceptable only as a last resort, never
/// attempted first.
fn open_for_sync(db: &Path) -> Option<rusqlite::Connection> {
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match rusqlite::Connection::open_with_flags(db, flags) {
        Ok(conn) => return Some(conn),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if matches!(
                err.code,
                rusqlite::ffi::ErrorCode::ReadOnly | rusqlite::ffi::ErrorCode::CannotOpen
            ) => {}
        Err(_) => return None,
    }
    let uri = sqlite_uri(db);
    let immutable_flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_URI
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    rusqlite::Connection::open_with_flags(&uri, immutable_flags).ok()
}

/// One column's value as JSON, or `None` for a blob (skipped -- see
/// `ShadowTranscript::sync_sqlite`'s doc comment).
fn column_value_json(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<serde_json::Value>> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(index)? {
        ValueRef::Null => Some(serde_json::Value::Null),
        ValueRef::Integer(i) => Some(serde_json::Value::from(i)),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map(serde_json::Value::Number),
        ValueRef::Text(t) => Some(serde_json::Value::String(
            String::from_utf8_lossy(t).into_owned(),
        )),
        ValueRef::Blob(_) => None,
    })
}

/// Renders `path` as a `file:` URI suitable for `SQLITE_OPEN_URI`, with
/// `?immutable=1` appended. Backslashes (a plain Windows path) are rewritten
/// to forward slashes -- SQLite's URI filename parser wants a URI, not an
/// OS-native path, and forward slashes are accepted as path separators by
/// the Windows file APIs SQLite calls underneath either way (SQLite's own
/// URI-filename documentation gives `file:C:/path/to/file` as the canonical
/// Windows example, with no leading slash before the drive letter). `?`,
/// `#`, `%` and space are percent-encoded because they are URI
/// metacharacters or otherwise unsafe unescaped in a URI (a literal `?`
/// would otherwise start the query string early); no other character is
/// touched, since every other byte a real filesystem path can contain is
/// legal inside a URI path component as SQLite parses it.
fn sqlite_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => uri.push('/'),
            ' ' | '?' | '#' | '%' => uri.push_str(&format!("%{:02X}", ch as u32)),
            other => uri.push(other),
        }
    }
    uri.push_str("?immutable=1");
    uri
}

/// Deletes every file in `dir` whose modified time is older than
/// `max_age_days`. Best-effort throughout, exactly like
/// [`state::prune_to_newest`]: a directory that cannot be read, a file whose
/// mtime cannot be read, or one that cannot be removed, is simply left
/// alone. Neither `prune_to_newest` (keeps a fixed COUNT, not an age) nor
/// `log::prune_safety_buckets` (parses a DAY out of the filename) fits a
/// shadow directory, whose files are named after an opaque per-session short
/// id rather than counted or day-bucketed -- so this reads each file's own
/// mtime instead, the one signal common to both of those approaches.
fn prune_idle_shadow_files(dir: &Path, max_age_days: u64) {
    let Some(cutoff) = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_days * 86_400))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::ScoreConfig;
    use crate::commands::ctx::event::{Capabilities, NormalizedEvent, SessionId};
    use crate::commands::ctx::score::IncrementalScorer;
    use std::process::Command;

    #[test]
    fn codex_final_message_extracts_only_assistant_output_text() {
        let jsonl = r#"
{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"old report"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"output_text","text":"not a report"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"final report"},{"type":"tool_call","arguments":"secret"},{"type":"output_text","text":"second paragraph"}]}}
{"type":"response_item","payload":{"type":"function_call","arguments":"secret"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"progress only"}]}}
{"partial":
"#;
        assert_eq!(
            codex_final_assistant_message(jsonl).as_deref(),
            Some("final report\nsecond paragraph")
        );
        assert_eq!(codex_final_assistant_message(""), None);
        assert_eq!(
            codex_final_assistant_message(
                r#"{"type":"response_item","payload":{"type":"function_call","arguments":"secret"}}"#
            ),
            None
        );
    }

    fn session(id: &str) -> SessionRef {
        SessionRef {
            id: SessionId::parse(id),
            cwd: PathBuf::from("/work/repo"),
        }
    }

    fn shadow_for(state_root: &Path, id: &str) -> ShadowTranscript {
        let state = StateDir::from_root(state_root.to_path_buf());
        ShadowTranscript::for_session(&state, &session(id))
    }

    fn extract_array(value: &serde_json::Value) -> Vec<serde_json::Value> {
        value.as_array().cloned().unwrap_or_default()
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    // -- sync_json_array ---------------------------------------------------

    #[test]
    fn the_first_sync_writes_every_row_as_one_line_each() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "11111111-1111-4111-8111-111111111111");
        let source = dir.path().join("snapshot.json");
        std::fs::write(
            &source,
            serde_json::to_string(&serde_json::json!([
                {"role": "user", "text": "a"},
                {"role": "assistant", "text": "b"},
                {"role": "user", "text": "c"},
            ]))
            .expect("json"),
        )
        .expect("write source");

        let path = shadow
            .sync_json_array(&source, &extract_array)
            .expect("sync");
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3, "one line per row: {lines:?}");
        assert!(lines[0].contains("\"a\""));
        assert!(lines[2].contains("\"c\""));
    }

    #[test]
    fn an_unchanged_source_appends_nothing_on_the_second_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "22222222-2222-4222-8222-222222222222");
        let source = dir.path().join("snapshot.json");
        let rows = serde_json::json!([{"role": "user", "text": "a"}]);
        std::fs::write(&source, rows.to_string()).expect("write source");

        shadow
            .sync_json_array(&source, &extract_array)
            .expect("first sync");
        let modified_after_first = std::fs::metadata(shadow.path())
            .expect("meta")
            .modified()
            .expect("mtime");

        // A real filesystem's mtime resolution can be coarse enough that a
        // same-instant no-op write would not even show up as a change --
        // sleeping past it makes the assertion meaningful either way.
        std::thread::sleep(std::time::Duration::from_millis(20));
        shadow
            .sync_json_array(&source, &extract_array)
            .expect("second sync");
        let modified_after_second = std::fs::metadata(shadow.path())
            .expect("meta")
            .modified()
            .expect("mtime");

        assert_eq!(read_lines(shadow.path()).len(), 1, "still just the one row");
        assert_eq!(
            modified_after_first, modified_after_second,
            "an unchanged source must not even touch the shadow file"
        );
    }

    #[test]
    fn an_extended_source_appends_only_the_new_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "33333333-3333-4333-8333-333333333333");
        let source = dir.path().join("snapshot.json");
        std::fs::write(
            &source,
            serde_json::json!([{"role": "user", "text": "a"}]).to_string(),
        )
        .expect("write source");
        shadow
            .sync_json_array(&source, &extract_array)
            .expect("first sync");

        std::fs::write(
            &source,
            serde_json::json!([
                {"role": "user", "text": "a"},
                {"role": "assistant", "text": "b"},
                {"role": "user", "text": "c"},
            ])
            .to_string(),
        )
        .expect("extend source");
        shadow
            .sync_json_array(&source, &extract_array)
            .expect("second sync");

        let lines = read_lines(shadow.path());
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("\"b\""));
        assert!(lines[2].contains("\"c\""));
    }

    #[test]
    fn a_shrunk_source_restarts_the_shadow_from_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "44444444-4444-4444-8444-444444444444");
        let source = dir.path().join("snapshot.json");
        std::fs::write(
            &source,
            serde_json::json!([
                {"role": "user", "text": "a"},
                {"role": "assistant", "text": "b"},
                {"role": "user", "text": "c"},
            ])
            .to_string(),
        )
        .expect("write source");
        shadow
            .sync_json_array(&source, &extract_array)
            .expect("first sync");
        assert_eq!(read_lines(shadow.path()).len(), 3);

        // A new session reusing the same snapshot file, or a genuine
        // truncation -- either way fewer rows than the shadow already holds.
        std::fs::write(
            &source,
            serde_json::json!([{"role": "user", "text": "z"}]).to_string(),
        )
        .expect("shrink source");
        shadow
            .sync_json_array(&source, &extract_array)
            .expect("restart sync");

        let lines = read_lines(shadow.path());
        assert_eq!(
            lines,
            vec![serde_json::json!({"role": "user", "text": "z"}).to_string()]
        );
    }

    /// The bug this redesign fixes (see `ShadowTranscript`'s own doc
    /// comment): a separate cursor file used to record how many rows were
    /// already written, and a crash between appending rows to the shadow
    /// and updating that cursor left it behind what the shadow already
    /// held, so the next sync trusted the stale cursor and replayed rows
    /// the shadow already had. There is no cursor file any more --
    /// `append_rows` is called directly here to put the shadow in exactly
    /// that "rows already landed, nothing else to update" state a crash
    /// would have left, then a normal sync of a source describing those
    /// same rows must find nothing new, because the shadow's own row count
    /// IS the position now.
    #[test]
    fn a_crash_that_left_rows_appended_never_replays_them_on_the_next_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "ffffffff-ffff-4fff-8fff-ffffffffffff");
        let rows = serde_json::json!([
            {"role": "user", "text": "a"},
            {"role": "assistant", "text": "b"},
        ]);
        shadow
            .append_rows(&extract_array(&rows))
            .expect("simulate a completed append with no cursor to record it");
        assert_eq!(read_lines(shadow.path()).len(), 2, "sanity: rows landed");

        let source = dir.path().join("snapshot.json");
        std::fs::write(&source, rows.to_string()).expect("write source");
        shadow
            .sync_json_array(&source, &extract_array)
            .expect("sync after the simulated crash");

        let lines = read_lines(shadow.path());
        assert_eq!(
            lines.len(),
            2,
            "the already-written rows must not be replayed: {lines:?}"
        );
    }

    /// The other half of the crash this redesign survives: a write that
    /// stopped mid-row (a crash inside `append_rows`'s own loop, between two
    /// `write_all` calls) leaves a trailing line with no final newline.
    /// `shadow_row_count` must not count it, and must drop it before the
    /// next row is appended, so the row it represents is re-synced whole
    /// rather than left as a permanently broken half-line.
    #[test]
    fn a_partial_trailing_line_is_dropped_and_the_row_is_re_synced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "12121212-1212-4121-8121-121212121212");
        let source = dir.path().join("snapshot.json");
        std::fs::write(
            &source,
            serde_json::json!([{"role": "user", "text": "a"}]).to_string(),
        )
        .expect("write source");
        shadow
            .sync_json_array(&source, &extract_array)
            .expect("first sync");
        assert_eq!(read_lines(shadow.path()).len(), 1);

        // Simulate a crash mid-append: a second row's line was started but
        // never terminated with a newline.
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(shadow.path())
                .expect("open for corruption");
            file.write_all(br#"{"role":"assistant"#)
                .expect("write partial line");
        }
        assert!(
            std::fs::read(shadow.path())
                .expect("read")
                .last()
                .is_some_and(|&b| b != b'\n'),
            "sanity: the shadow now ends in a partial line"
        );

        std::fs::write(
            &source,
            serde_json::json!([
                {"role": "user", "text": "a"},
                {"role": "assistant", "text": "b"},
            ])
            .to_string(),
        )
        .expect("extend source");
        shadow
            .sync_json_array(&source, &extract_array)
            .expect("second sync");

        let lines = read_lines(shadow.path());
        assert_eq!(
            lines.len(),
            2,
            "the partial line must be dropped and the row re-synced whole: {lines:?}"
        );
        for line in &lines {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "every line must be valid JSON, proving the partial fragment is gone: {line}"
            );
        }
        assert!(lines[1].contains("\"b\""));
    }

    #[test]
    fn a_missing_source_leaves_the_shadow_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "55555555-5555-4555-8555-555555555555");
        let missing = dir.path().join("gone.json");

        let path = shadow
            .sync_json_array(&missing, &extract_array)
            .expect("no error on a missing source");
        assert_eq!(path, shadow.path());
        assert!(!path.exists(), "nothing was ever written");
    }

    #[test]
    fn an_unparseable_source_leaves_the_shadow_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "66666666-6666-4666-8666-666666666666");
        let source = dir.path().join("snapshot.json");
        std::fs::write(&source, "{ not json").expect("write garbage");

        let path = shadow
            .sync_json_array(&source, &extract_array)
            .expect("no error on unparseable source");
        assert!(!path.exists());
    }

    #[test]
    fn a_snapshot_fixture_round_trips_through_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "77777777-7777-4777-8777-777777777777");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/shadow/snapshot.json");

        let path = shadow
            .sync_json_array(&fixture, &|value| {
                value
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .map(
                                |row| serde_json::json!({"role": row["role"], "text": row["text"]}),
                            )
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .expect("sync fixture");
        assert_eq!(read_lines(&path).len(), 3);
    }

    // -- sync_sqlite ---------------------------------------------------

    const MESSAGE_QUERY: &str =
        "SELECT rowid, role, data FROM message WHERE rowid > ?1 ORDER BY rowid";

    fn new_message_db(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).expect("open db");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE message (role TEXT NOT NULL, data TEXT NOT NULL);",
        )
        .expect("create table");
        conn
    }

    fn insert_message(conn: &rusqlite::Connection, role: &str, data: &str) {
        conn.execute(
            "INSERT INTO message (role, data) VALUES (?1, ?2)",
            rusqlite::params![role, data],
        )
        .expect("insert");
    }

    #[test]
    fn sqlite_sync_appends_only_rows_past_the_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let conn = new_message_db(&db_path);
        insert_message(&conn, "user", "a");
        insert_message(&conn, "assistant", "b");

        let shadow = shadow_for(dir.path(), "88888888-8888-4888-8888-888888888888");
        shadow
            .sync_sqlite(&db_path, MESSAGE_QUERY)
            .expect("first sync");
        assert_eq!(read_lines(shadow.path()).len(), 2);

        insert_message(&conn, "user", "c");
        shadow
            .sync_sqlite(&db_path, MESSAGE_QUERY)
            .expect("second sync");
        let lines = read_lines(shadow.path());
        assert_eq!(lines.len(), 3, "only the new row was appended: {lines:?}");
        assert!(lines[2].contains("\"c\""));
        assert!(lines[2].contains("\"rowid\""));
    }

    /// The SQLite half of the crash this redesign fixes (see
    /// `ShadowTranscript`'s own doc comment): `append_rows` here simulates
    /// rows a crashed sync already landed in the shadow, with no separate
    /// cursor file left behind to fall out of sync with it. A normal sync
    /// against a database whose rows are the same ones already in the
    /// shadow must find nothing past `shadow_last_rowid` and append
    /// nothing.
    #[test]
    fn a_crash_that_left_sqlite_rows_appended_never_replays_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let conn = new_message_db(&db_path);
        insert_message(&conn, "user", "a");
        insert_message(&conn, "assistant", "b");

        let shadow = shadow_for(dir.path(), "13131313-1313-4131-8131-131313131313");
        shadow
            .append_rows(&[
                serde_json::json!({"rowid": 1, "role": "user", "data": "a"}),
                serde_json::json!({"rowid": 2, "role": "assistant", "data": "b"}),
            ])
            .expect("simulate a completed append with no cursor to record it");
        assert_eq!(read_lines(shadow.path()).len(), 2, "sanity: rows landed");

        shadow
            .sync_sqlite(&db_path, MESSAGE_QUERY)
            .expect("sync after the simulated crash");
        assert_eq!(
            read_lines(shadow.path()).len(),
            2,
            "the already-written rows must not be replayed"
        );
    }

    /// The same partial-line hazard `sync_json_array` repairs, exercised
    /// through `sync_sqlite`: a trailing line with no final newline must be
    /// dropped and `shadow_last_rowid` must read `0`, not the broken line's
    /// (absent) rowid, so the row it represents is re-fetched from the
    /// database rather than left as a permanently broken half-line.
    #[test]
    fn a_partial_trailing_sqlite_line_is_dropped_and_the_row_is_re_synced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let conn = new_message_db(&db_path);
        insert_message(&conn, "user", "a");

        let shadow = shadow_for(dir.path(), "14141414-1414-4141-8141-141414141414");
        shadow
            .sync_sqlite(&db_path, MESSAGE_QUERY)
            .expect("first sync");
        assert_eq!(read_lines(shadow.path()).len(), 1);

        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(shadow.path())
                .expect("open for corruption");
            file.write_all(br#"{"rowid":2,"role":"assistant"#)
                .expect("write partial line");
        }

        insert_message(&conn, "assistant", "b");
        shadow
            .sync_sqlite(&db_path, MESSAGE_QUERY)
            .expect("second sync");

        let lines = read_lines(shadow.path());
        assert_eq!(
            lines.len(),
            2,
            "the partial line must be dropped and the row re-synced whole: {lines:?}"
        );
        for line in &lines {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "every line must be valid JSON, proving the partial fragment is gone: {line}"
            );
        }
        assert!(lines[1].contains("\"b\""));
    }

    /// The whole point of preferring a plain read-only open over
    /// `?immutable=1` (see `open_for_sync`'s doc comment): `immutable=1`
    /// never attaches the `-wal` file, so a row a writer committed but has
    /// not yet checkpointed into the main database would stay invisible for
    /// up to a full checkpoint interval. A plain `SQLITE_OPEN_READ_ONLY`
    /// connection reads through the WAL like any ordinary reader, so it
    /// must see this row immediately -- no `PRAGMA wal_checkpoint` anywhere
    /// in this test.
    #[test]
    fn a_committed_uncheckpointed_wal_row_is_visible_to_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let conn = new_message_db(&db_path);
        insert_message(&conn, "user", "a");

        let shadow = shadow_for(dir.path(), "dddddddd-dddd-4ddd-8ddd-dddddddddddd");
        shadow.sync_sqlite(&db_path, MESSAGE_QUERY).expect("sync");
        let lines = read_lines(shadow.path());
        assert_eq!(
            lines.len(),
            1,
            "a committed but uncheckpointed WAL row must be visible: {lines:?}"
        );
        assert!(lines[0].contains("\"a\""));
    }

    #[test]
    fn a_missing_database_leaves_the_shadow_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "99999999-9999-4999-8999-999999999999");
        let missing = dir.path().join("gone.db");

        let path = shadow
            .sync_sqlite(&missing, MESSAGE_QUERY)
            .expect("no error on a missing db");
        assert!(!path.exists());
    }

    /// In WAL mode a reader never blocks a writer and a writer never blocks
    /// a reader -- proven here by holding a write transaction open on a
    /// second, ordinary connection at the same time as the sync, with the
    /// sync required to complete rather than block on that writer's lock.
    #[test]
    fn an_open_write_transaction_elsewhere_never_blocks_the_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let conn = new_message_db(&db_path);
        insert_message(&conn, "user", "a");

        let writer = rusqlite::Connection::open(&db_path).expect("second connection");
        writer
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO message (role, data) VALUES ('assistant', 'held-open');")
            .expect("open write transaction");

        // Owned copies for the spawned thread -- `dir` (the `TempDir` guard)
        // must stay alive in THIS scope until the assertions below have run,
        // never moved into the thread, or its `Drop` could delete the whole
        // directory out from under this test's final read.
        let session_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let state_root_for_thread = dir.path().to_path_buf();
        let db_path_for_thread = db_path.clone();
        let shadow = shadow_for(dir.path(), session_id);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let state = StateDir::from_root(state_root_for_thread);
            let shadow = ShadowTranscript::for_session(
                &state,
                &SessionRef {
                    id: SessionId::parse(session_id),
                    cwd: PathBuf::from("/work/repo"),
                },
            );
            let result = shadow.sync_sqlite(&db_path_for_thread, MESSAGE_QUERY);
            let _ = tx.send(result.is_ok());
        });

        let finished = rx.recv_timeout(std::time::Duration::from_secs(5));
        writer.execute_batch("ROLLBACK;").expect("rollback");
        assert_eq!(
            finished,
            Ok(true),
            "the sync must complete without waiting on the open write transaction"
        );
        assert_eq!(
            read_lines(shadow.path()).len(),
            1,
            "sanity: baseline row synced"
        );
    }

    // -- shadow scores through the rot engine ------------------------------

    /// Modelled on `EventlessAdapter` in `score.rs`'s own tests, with
    /// `capabilities().events` turned on and a real `parse_events` -- the
    /// minimal shape needed to prove a synced shadow file scores like any
    /// other transcript through `IncrementalScorer::poll`.
    #[derive(Debug)]
    struct StubAdapter;

    impl crate::commands::ctx::adapters::AgentAdapter for StubAdapter {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn program(&self) -> &str {
            "stub"
        }
        fn provider(&self) -> &'static str {
            "stub"
        }
        fn ready(&self) -> CtxResult<()> {
            Ok(())
        }
        fn detect(&self, _command: &[String]) -> bool {
            false
        }
        fn headless_cmd(&self, _prompt: &str, _session: &SessionId, _extra: &[String]) -> Command {
            Command::new("true")
        }
        fn interactive_cmd(&self, _initial_prompt: Option<&str>, _extra: &[String]) -> Command {
            Command::new("true")
        }
        fn distiller_cmd(&self, _model: &str) -> Command {
            Command::new("true")
        }
        fn read_only_args(&self) -> Vec<String> {
            Vec::new()
        }
        fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
            Vec::new()
        }
        fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
            PathBuf::new()
        }
        fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent> {
            jsonl
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .map(|value| NormalizedEvent::AssistantFinal {
                    text: value["text"].as_str().unwrap_or_default().to_string(),
                    input_tokens: 1,
                    at_ms: None,
                })
                .collect()
        }
        fn structural_context(
            &self,
            _jsonl: &str,
            _last_n: usize,
        ) -> crate::commands::ctx::event::StructuralContext {
            crate::commands::ctx::event::StructuralContext::default()
        }
        fn compact_command(&self) -> Option<&'static str> {
            None
        }
        fn quit_sequence(&self) -> &'static str {
            ""
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                events: true,
                ..Capabilities::default()
            }
        }
        fn register_turn_signal(
            &self,
            _session: &SessionRef,
            _socket: &Path,
        ) -> crate::commands::ctx::adapters::TurnSignalSetup {
            crate::commands::ctx::adapters::TurnSignalSetup {
                env: Vec::new(),
                instructions: String::new(),
            }
        }
    }

    #[test]
    fn a_synced_shadow_scores_through_the_incremental_scorer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow = shadow_for(dir.path(), "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        let source = dir.path().join("snapshot.json");
        std::fs::write(
            &source,
            serde_json::json!([
                {"role": "assistant", "text": "hello from the shadow"},
            ])
            .to_string(),
        )
        .expect("write source");

        let path = shadow
            .sync_json_array(&source, &extract_array)
            .expect("sync");

        let adapter = StubAdapter;
        let mut scorer = IncrementalScorer::new(path);
        let (score, _screening) = scorer
            .poll(
                &adapter,
                &ScoreConfig::default(),
                &crate::commands::ctx::screen::Thresholds::default(),
            )
            .expect("poll");
        assert!(
            score.is_some(),
            "a synced shadow line must score exactly like any other transcript line"
        );
    }
}
