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
/// `<short>.jsonl`/`<short>.cursor` pair per SESSION -- there is no day to
/// parse out of a short session id -- so pruning here reads each file's own
/// mtime instead, the same signal [`state::prune_to_newest`] reads, just
/// compared against an age cutoff rather than a keep-count.
const SHADOW_RETENTION_DAYS: u64 = 30;

/// One session's materialized shadow transcript: `<state>/shadow/<short>.jsonl`
/// holds the JSONL rows the rot engine reads, and `<short>.cursor` holds how
/// many source rows (JSON array) or the highest `rowid` (SQLite) are already
/// reflected in it. A sibling of [`StateDir::rollouts`], with the same
/// short-id derivation ([`sessions::short_id`]).
pub struct ShadowTranscript {
    jsonl: PathBuf,
    cursor: PathBuf,
}

impl ShadowTranscript {
    /// Ensures `<state>/shadow` exists with the same private-directory
    /// discipline every other state-dir subdirectory gets
    /// ([`state::create_private_dir_all`]), prunes shadow files idle for
    /// more than [`SHADOW_RETENTION_DAYS`], then names this session's pair of
    /// files inside it. Both steps are best-effort, matching every other
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
            cursor: dir.join(format!("{short}.cursor")),
        }
    }

    /// The shadow JSONL path -- what an adapter's `transcript_path` returns
    /// after syncing.
    pub fn path(&self) -> &Path {
        &self.jsonl
    }

    /// Reads `self.cursor` as a single decimal number of type `T`. `None`
    /// covers both a missing file (the ordinary first-sync case) and one
    /// that fails to parse (a corrupted or foreign file) -- both read as "no
    /// cursor on record", which both `sync_json_array` and `sync_sqlite`
    /// treat identically: rewrite the shadow from scratch rather than trust
    /// a shadow file whose cursor cannot be reconciled with it.
    fn read_cursor<T: std::str::FromStr>(&self) -> Option<T> {
        std::fs::read_to_string(&self.cursor)
            .ok()
            .and_then(|text| text.trim().parse::<T>().ok())
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
        // `for_session` always names both files as direct children of the
        // same shadow directory, so either one's parent is that directory.
        self.jsonl.parent().unwrap_or(Path::new("."))
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
    /// cursor file holds how many of those rows are already written. A
    /// snapshot that now has FEWER rows than the cursor remembers -- a new
    /// session reusing the same file, or a genuine truncation -- rewrites
    /// the shadow from zero, which `Watcher::read_appended` already reads as
    /// a restart (see its own doc comment: same-or-shorter length is never
    /// treated as an append). A missing cursor file, or one that fails to
    /// parse, reads the same way: [`Self::read_cursor`] cannot tell "never
    /// synced" apart from "cursor lost its meaning", so both take the safe
    /// path and rewrite from scratch rather than risk silently duplicating
    /// or skipping rows against a shadow file the cursor can no longer
    /// account for.
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

        let stored = self.read_cursor::<usize>();
        let needs_reset = stored.is_none() || rows.len() < stored.unwrap_or(0);
        let cursor = if needs_reset { 0 } else { stored.unwrap_or(0) };

        if needs_reset || rows.len() > cursor {
            if needs_reset {
                self.reset()?;
            }
            self.append_rows(&rows[cursor..])?;
            state::write_private(&self.cursor, &rows.len().to_string())?;
        }
        Ok(self.jsonl.clone())
    }

    /// Syncs rows past the cursor from a harness's SQLite transcript into
    /// the shadow JSONL.
    ///
    /// Opens the database read-only, as a URI (`SQLITE_OPEN_URI`) with
    /// `?immutable=1`: SQLite takes this as a promise that the file will not
    /// change while the handle is open and skips locking entirely, so a live
    /// writer is never blocked by this read and this read never waits on it
    /// either -- at the cost of a possibly slightly stale snapshot (a commit
    /// mid-poll may not be visible yet), which the next poll picks up like
    /// any other incremental read.
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
    /// base64 dependency for bytes nothing downstream consumes. The cursor
    /// file stores the highest rowid seen, exactly like `sync_json_array`'s
    /// cursor stores a row count (see [`Self::read_cursor`]'s doc comment
    /// for how a missing/unparseable cursor is handled identically here:
    /// treated as zero and the shadow rewritten from scratch).
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

        let stored = self.read_cursor::<i64>();
        let needs_reset = stored.is_none();
        let cursor = stored.unwrap_or(0);

        let uri = sqlite_uri(db);
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let Ok(conn) = rusqlite::Connection::open_with_flags(&uri, flags) else {
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
        let mut max_rowid = cursor;
        // A fault partway through iteration (e.g. a concurrent checkpoint)
        // simply stops the scan here -- whatever was already read is kept,
        // never discarded, and the next poll picks up past `max_rowid`.
        while let Some(row) = rows.next().ok().flatten() {
            let Ok(rowid) = row.get::<_, i64>(0) else {
                continue;
            };
            max_rowid = max_rowid.max(rowid);
            let mut obj = serde_json::Map::new();
            obj.insert("rowid".to_string(), serde_json::Value::from(rowid));
            for (index, name) in column_names.iter().enumerate().skip(1) {
                if let Ok(Some(value)) = column_value_json(row, index) {
                    obj.insert(name.clone(), value);
                }
            }
            new_rows.push(serde_json::Value::Object(obj));
        }

        if needs_reset || !new_rows.is_empty() {
            if needs_reset {
                self.reset()?;
            }
            self.append_rows(&new_rows)?;
            state::write_private(&self.cursor, &max_rowid.to_string())?;
        }
        Ok(self.jsonl.clone())
    }
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
        // truncation -- either way fewer rows than the cursor remembers.
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
        // Checkpoint so the immutable reader below (which does not attach
        // the WAL) sees these committed rows in the main database file.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");

        let shadow = shadow_for(dir.path(), "88888888-8888-4888-8888-888888888888");
        shadow
            .sync_sqlite(&db_path, MESSAGE_QUERY)
            .expect("first sync");
        assert_eq!(read_lines(shadow.path()).len(), 2);

        insert_message(&conn, "user", "c");
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");
        shadow
            .sync_sqlite(&db_path, MESSAGE_QUERY)
            .expect("second sync");
        let lines = read_lines(shadow.path());
        assert_eq!(lines.len(), 3, "only the new row was appended: {lines:?}");
        assert!(lines[2].contains("\"c\""));
        assert!(lines[2].contains("\"rowid\""));
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

    /// `?immutable=1` promises SQLite the file will not change while this
    /// handle is open and skips locking entirely -- proven here by holding a
    /// write transaction open on a second, ordinary connection at the same
    /// time as the immutable sync, with the sync required to complete
    /// rather than block on that writer's lock.
    #[test]
    fn an_open_write_transaction_elsewhere_never_blocks_the_immutable_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let conn = new_message_db(&db_path);
        insert_message(&conn, "user", "a");
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");

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
            "the immutable read must complete without waiting on the open write transaction"
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
