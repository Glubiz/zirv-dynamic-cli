use std::path::{Path, PathBuf};

use super::CtxResult;
use super::config::EnvLookup;

pub const STATE_ENV: &str = "ZIRV_CTX_STATE_DIR";

/// Seconds since the unix epoch. Zero-padded decimal seconds sort
/// lexicographically in chronological order, which is how handoffs and log
/// lines stay ordered without a date library.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Canonicalizes first, then replaces every character outside `[A-Za-z0-9-]`
/// with `-` -- the same rule the claude adapter uses for transcript
/// directories.
///
/// The canonicalization is what makes this a single answer per repository.
/// Callers reach it from both sides: some pass a path they canonicalized
/// (`artifact::register`), some pass the raw `--repo` value or a bare
/// `current_dir()` (`workflow stats`, verification reports, workflow state).
/// On a machine where the two spellings differ -- macOS's `/var` ->
/// `/private/var`, any symlinked checkout -- that split one repository's state
/// across two slugs, so events were written where the reader never looked. A
/// path that cannot be canonicalized (it does not exist yet, or is not
/// readable) falls back to its own text, which is the pre-existing behavior.
pub fn repo_slug(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    display_path(&path)
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

/// Strips a Windows verbatim path prefix (`\\?\`, or `\\?\UNC\` for a UNC
/// path) for user-facing display. `std::fs::canonicalize` yields a verbatim
/// path on Windows (`\\?\D:\...`) -- correct for filesystem calls, but
/// confusing and often un-copy-pasteable printed to a terminal (`setup.rs`
/// was doing this raw: "Zirv AI setup for \\?\D:\..."). A no-op on
/// non-Windows and on any path that was never in verbatim form. `repo_slug`
/// above uses this same stripping so callers get one answer, not two
/// (2026-08-23, issue #101).
pub fn display_path(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    #[cfg(windows)]
    let rendered = if let Some(rest) = rendered.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        rendered
            .strip_prefix(r"\\?\")
            .unwrap_or(rendered.as_ref())
            .to_string()
    };
    #[cfg(not(windows))]
    let rendered = rendered.into_owned();
    rendered
}

/// Filesystem-safe form of an adapter's provider slug
/// (`AgentAdapter::provider`), for the per-provider usage files. Lowercased
/// first, then every character outside `[a-z0-9-]` replaced with `-`, the
/// same shape as `repo_slug` above: a provider name is a `&'static str` an
/// adapter chose, but the rule is what guarantees it can never carry a
/// separator or a `..` out of the state directory. An empty result (a slug
/// of nothing but punctuation) becomes `unknown` rather than an empty file
/// name.
pub fn provider_slug(provider: &str) -> String {
    let slug: String = provider
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        return "unknown".to_string();
    }
    slug
}

/// The state dir holds transcript paths, prompts, distilled handoffs and a
/// decision log: on a shared machine, none of that is anyone else's business.
/// Directories are created 0700 and files 0600. Both are no-ops on Windows,
/// which has no equivalent single call, and neither touches a path that
/// already exists.
#[cfg(unix)]
pub fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(not(unix))]
pub fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(unix)]
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// A unique temp sibling of `target`, in the *same* directory so the `rename`
/// in `write_private` is a same-filesystem atomic replace. The pid plus a
/// process-local counter keeps two concurrent writers -- or two writes from
/// one process -- from ever colliding on the same temp path.
fn temp_sibling(target: &Path) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let name = match target.file_name().and_then(|n| n.to_str()) {
        Some(base) => format!(".{base}.tmp-{pid}-{n}"),
        None => format!(".tmp-{pid}-{n}"),
    };
    match target.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Shared machinery behind `write_private`/`write_shared`: write to a temp
/// sibling, then `rename` it over `path` (atomic on the same filesystem, on
/// both Windows and Unix), so a concurrent reader ever sees either the whole
/// old file or the whole new one -- never the zero-length truncation window a
/// plain create-truncate-write leaves. That window was a real hazard: a
/// session refreshing its registry record while a dashboard `sessions::list`
/// read it could have the record read as absent (and its pending nudge
/// swept). `rename` replaces the directory entry itself rather than writing
/// through it, so this is also safe when `path` already exists as a symlink:
/// the link is replaced by a regular file, never dereferenced and written
/// through to wherever it pointed.
///
/// `force_owner_only` applies `write_private`'s 0600-regardless-of-umask
/// hardening; `write_shared` leaves it off, since that content lives in a
/// normal repository checkout and should get ordinary, umask-respecting
/// permissions like any other file zirv writes into a checkout, not the
/// machine-local-secret treatment state-dir content gets. Unix-only in
/// effect: the permission bits it gates don't exist on Windows, so the
/// parameter is genuinely unused on that target, not merely unread by
/// omission.
#[cfg_attr(not(unix), allow(unused_variables))]
fn write_atomic(path: &Path, contents: &str, force_owner_only: bool) -> std::io::Result<()> {
    use std::io::Write;

    let tmp = temp_sibling(path);

    // Write (and close) the temp file first; the handle must be dropped before
    // the rename on Windows.
    let write_tmp = || -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        if force_owner_only {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        #[cfg(unix)]
        if force_owner_only {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(contents.as_bytes())?;
        file.flush()
    };

    if let Err(e) = write_tmp() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Writes `contents` to `path` atomically. On Unix the file is 0600, forced
/// on the fresh temp regardless of umask, so writing over an operator's
/// pre-existing world-readable file still yields a private one.
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    write_atomic(path, contents, true)
}

/// Same atomic temp-sibling-then-`rename` guarantee as `write_private`, for
/// content meant to live in the repository checkout itself
/// (`<repo>/.zirv/memory/`, the shared memory scope) rather than the
/// machine-local state dir. Deliberately does NOT force 0600: a shared-scope
/// file is ordinary, human-editable repository content and should get
/// whatever permissions the process umask would give any other file zirv
/// writes into a checkout (the same convention `zirv init` already uses for
/// `.zirv/` itself), not `write_private`'s "private machine secret"
/// treatment. Not yet called from any non-test code -- `memory::upsert_shared`
/// is its first consumer, itself dormant until `zirv memory` (Task 3) wires a
/// CLI verb on top of it.
#[allow(dead_code)]
pub fn write_shared(path: &Path, contents: &str) -> std::io::Result<()> {
    write_atomic(path, contents, false)
}

/// How many files the per-session directories keep. High enough that no live
/// session is ever pruned out from under itself, low enough that a machine
/// running zirv for months does not accumulate one file per session forever.
pub const KEEP_NEWEST: usize = 200;

/// Drops all but the `keep` newest files in `dir`. Best-effort in every
/// direction: a directory that cannot be read, a file whose mtime cannot be
/// read, or one that cannot be removed, is simply left alone. Housekeeping
/// must never be the reason a session fails to start.
///
/// Only for directories zirv writes one file per session into. Handoffs, logs
/// and reports have their own retention and are not touched.
pub fn prune_to_newest(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file()
                .then(|| Some((meta.modified().ok()?, entry.path())))?
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    // Newest first, so everything past `keep` is the oldest.
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files.iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl StateDir {
    /// Test seam: production always goes through `resolve`.
    #[cfg(test)]
    pub fn from_root(root: PathBuf) -> Self {
        Self(root)
    }

    /// `ZIRV_CTX_STATE_DIR`, else the platform state dir, else the platform
    /// local data dir (macOS and Windows have no state dir), plus `zirv/ctx`.
    pub fn resolve(env: EnvLookup<'_>) -> CtxResult<Self> {
        if let Some(raw) = env(STATE_ENV) {
            return Ok(Self(PathBuf::from(raw)));
        }
        let base = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .ok_or("could not determine a platform state directory")?;
        Ok(Self(base.join("zirv").join("ctx")))
    }

    pub fn root(&self) -> &Path {
        &self.0
    }

    pub fn handoffs(&self) -> PathBuf {
        self.0.join("handoffs")
    }

    pub fn optimize_reports(&self) -> PathBuf {
        self.0.join("optimize")
    }

    /// Inter-agent mailbox: `<state>/mail/<repo_slug>/...`. See
    /// `super::mail` for the storage layout and message format.
    pub fn mail(&self) -> PathBuf {
        self.0.join("mail")
    }

    /// Cross-session memory bank: `<state>/memory/<repo_slug>/...`. See
    /// `super::memory` for the storage layout and entry format.
    pub fn memory(&self) -> PathBuf {
        self.0.join("memory")
    }

    /// Durable provider-neutral workflow state. Each repository gets an
    /// isolated slug directory; workflow prompts contain only the current
    /// step, while completed-step state stays here across compaction/restart.
    pub fn workflows(&self) -> PathBuf {
        self.0.join("workflows")
    }

    /// Structured verification evidence, separate from workflow state so a
    /// check run can be used before a workflow exists and referenced by later
    /// review/verification phases without embedding its raw logs in prompts.
    pub fn verification(&self) -> PathBuf {
        self.0.join("verification")
    }

    /// Artifact metadata and stable IDs. Artifact payloads remain normal
    /// repository/static files; only compact references are persisted here.
    pub fn artifacts(&self) -> PathBuf {
        self.0.join("artifacts")
    }

    /// Privacy-conscious workflow telemetry. Each event is a bounded
    /// structured record; prompts, source code, and model responses never
    /// enter this tree.
    pub fn workflow_telemetry(&self) -> PathBuf {
        self.0.join("workflow-telemetry")
    }

    /// Autonomous frontend profiles and visual evidence. Profiles are local
    /// derived state: repository files remain the source of truth and are
    /// never modified while Zirv infers a design direction.
    pub fn frontend(&self) -> PathBuf {
        self.0.join("frontend")
    }

    /// The dashboard's own state: today, only the spawn-request capability-
    /// token directories `super::dash::spawnreq::request_dir_for` names
    /// under `<state>/dash/<dash_short>-<token>/requests`. A future roster
    /// file (`super::dash::roster`) hangs off this same root.
    pub fn dash(&self) -> PathBuf {
        self.0.join("dash")
    }

    /// Short on purpose: unix socket paths are capped near 104 bytes on macOS.
    pub fn sockets(&self) -> PathBuf {
        self.0.join("s")
    }

    pub fn logs(&self) -> PathBuf {
        self.0.join("logs")
    }

    /// Machine-wide usage-window state, shared by every session that runs the
    /// statusline tee. One file, not per-session: the windows are per account.
    pub fn usage(&self) -> PathBuf {
        self.0.join("usage.json")
    }

    /// Per-provider usage-window state: `<state>/usage-<provider>.json`. The
    /// windows are per *account*, and one machine can hold accounts with two
    /// different vendors at once (an Anthropic subscription and an OpenAI
    /// one), which the single `usage()` file above cannot represent. The
    /// slug is sanitised by [`provider_slug`], so no provider name can name a
    /// path outside this directory.
    pub fn usage_for(&self, provider: &str) -> PathBuf {
        self.0
            .join(format!("usage-{}.json", provider_slug(provider)))
    }

    /// Poll-marker file: `<state>/poll-<provider>.json`, one `{"last_attempt":
    /// u64}` per provider. `poll::maybe_poll` uses this to throttle real
    /// network polls to `poll_min_interval_secs`, independent of how stale the
    /// stored usage reading looks -- a failed attempt still writes the marker,
    /// so a provider with no working token does not retry every call. The slug
    /// is sanitised by [`provider_slug`], mirroring [`Self::usage_for`].
    #[allow(dead_code)]
    pub fn poll_marker_for(&self, provider: &str) -> PathBuf {
        self.0
            .join(format!("poll-{}.json", provider_slug(provider)))
    }

    /// Per-transcript scoring checkpoints. The Stop hook is a fresh process on
    /// every turn, so the only place it can leave its parse position is a file.
    pub fn scoring(&self) -> PathBuf {
        self.0.join("scoring")
    }

    /// Session registry: `<state>/sessions/<short8>.json`, one file per live
    /// supervisor. See `super::sessions` for the record format and the short
    /// id derivation, which matches `socket_for`'s own exactly.
    pub fn sessions(&self) -> PathBuf {
        self.0.join("sessions")
    }

    /// `<state>/groups` -- one JSON file per work group (issue #155). A
    /// sibling of `sessions()`, and deliberately NOT inside it: a session is
    /// a live process, a group outlives every process in it and is the
    /// record of what a batch of delegated work was launched under.
    pub fn groups(&self) -> PathBuf {
        self.0.join("groups")
    }

    /// Issue #178: captured operator-approved permission prompts, ready for
    /// `permissions::propose`'s safe-list classifier -- `<state>/approvals/
    /// *.jsonl`, one file per day (see `log::append_safety`'s own doc
    /// comment for why day-bucketing: a hard time boundary without a
    /// cross-process truncate race between concurrent writers). A sibling of
    /// `logs()`, not inside it: these are the operator's own approval
    /// decisions distilled from transcripts, not a hook's own rotation
    /// verdicts.
    pub fn approvals(&self) -> PathBuf {
        self.0.join("approvals")
    }

    /// First 8 hex characters of the session id keep the socket path short.
    pub fn socket_for(&self, session: &str) -> PathBuf {
        let short: String = session
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
        self.sockets().join(format!("{short}.sock"))
    }

    /// Test seam: production creates each subdirectory as it first writes
    /// to it, so nothing needs the whole tree up front.
    #[cfg(test)]
    pub fn ensure(&self) -> CtxResult<()> {
        create_private_dir_all(&self.handoffs())?;
        create_private_dir_all(&self.sockets())?;
        create_private_dir_all(&self.logs())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn env_override_wins_and_paths_hang_off_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: HashMap<String, String> =
            [(STATE_ENV.to_string(), tmp.path().display().to_string())].into();
        let state = StateDir::resolve(&|k| env.get(k).cloned()).expect("resolve");

        assert_eq!(state.root(), tmp.path());
        assert_eq!(state.handoffs(), tmp.path().join("handoffs"));
        assert_eq!(state.sockets(), tmp.path().join("s"));
        assert_eq!(state.logs(), tmp.path().join("logs"));

        state.ensure().expect("ensure");
        assert!(state.handoffs().is_dir());
        assert!(state.sockets().is_dir());
        assert!(state.logs().is_dir());
    }

    /// M6 only held for files zirv created. Writing over one that already
    /// existed kept whatever permissions it had, so `--out` onto a path the
    /// operator had touched first produced a world-readable report full of
    /// transcript excerpts.
    #[cfg(unix)]
    #[test]
    fn writing_over_an_existing_file_still_makes_it_private() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("report.md");
        std::fs::write(&path, "placeholder").expect("pre-create");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        write_private(&path, "secrets").expect("write");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "an existing file is made private too");
    }

    /// One file per session, and nothing else ever removed them: the scoring
    /// checkpoints and prompt files grew for the life of the machine.
    #[test]
    fn pruning_keeps_the_newest_and_drops_the_rest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let base = std::time::SystemTime::now();

        for index in 0..10u32 {
            let path = dir.join(format!("{index}.json"));
            std::fs::write(&path, "x").expect("write");
            // Explicit mtimes: the filesystem's own resolution is too coarse
            // to order ten writes made in the same instant.
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open")
                .set_modified(base + std::time::Duration::from_secs(index as u64))
                .expect("set_modified");
        }

        prune_to_newest(dir, 3);

        let mut left: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(left, vec!["7.json", "8.json", "9.json"]);
    }

    #[test]
    fn pruning_a_directory_that_is_not_there_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        prune_to_newest(&tmp.path().join("absent"), 3);
    }

    #[test]
    fn default_root_ends_with_zirv_ctx() {
        let env: HashMap<String, String> = HashMap::new();
        let state = StateDir::resolve(&|k| env.get(k).cloned()).expect("resolve");
        assert!(
            state.root().ends_with("zirv/ctx"),
            "got {}",
            state.root().display()
        );
    }

    #[test]
    fn socket_paths_stay_short_enough_for_macos() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let path = state.socket_for("00000000-0000-4000-8000-000000000001");
        assert!(
            path.ends_with(std::path::Path::new("s").join("00000000.sock")),
            "got {}",
            path.display()
        );
    }

    #[test]
    fn repo_slug_is_filesystem_safe() {
        assert_eq!(
            repo_slug(std::path::Path::new("/Users/x/Documents/my repo.git")),
            "-Users-x-Documents-my-repo-git"
        );
    }

    // -- display_path (issue #101) ---------------------------------------

    #[cfg(windows)]
    #[test]
    fn display_path_strips_the_windows_verbatim_prefix() {
        assert_eq!(display_path(std::path::Path::new(r"\\?\D:\x\y")), r"D:\x\y");
    }

    #[cfg(windows)]
    #[test]
    fn display_path_strips_the_windows_verbatim_unc_prefix() {
        assert_eq!(
            display_path(std::path::Path::new(r"\\?\UNC\srv\share\x")),
            r"\\srv\share\x"
        );
    }

    #[test]
    fn display_path_leaves_a_plain_path_alone() {
        let plain = std::path::Path::new("some/plain/path");
        assert_eq!(display_path(plain), plain.to_string_lossy());
    }

    #[cfg(windows)]
    #[test]
    fn repo_slug_matches_claudes_transcript_slug_after_canonicalization() {
        let repo = crate::commands::ctx::testenv::repo();
        assert_eq!(
            repo_slug(repo.path()),
            super::super::adapters::claude::project_slug(repo.path())
        );
    }

    /// One repository, one slug, whichever spelling of its path a caller
    /// happens to hold. Callers reach this from both sides -- a canonicalized
    /// path in one place, a raw `--repo` value or `current_dir()` in another --
    /// and a split slug means events written where the reader never looks.
    #[cfg(unix)]
    #[test]
    fn two_spellings_of_one_repository_share_a_slug() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("repo");
        std::fs::create_dir(&real).expect("mkdir");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert_eq!(repo_slug(&link), repo_slug(&real));
        assert_eq!(
            repo_slug(&real.join(".")),
            repo_slug(&real),
            "a path that needs normalising resolves to the same slug"
        );
        // A path that does not exist keeps its own text, as before.
        let missing = tmp.path().join("gone");
        assert_eq!(repo_slug(&missing), {
            let mut expected = String::new();
            for ch in missing.to_string_lossy().chars() {
                expected.push(if ch.is_ascii_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '-'
                });
            }
            expected
        });
    }

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .permissions()
            .mode()
            & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn the_state_directory_is_private_to_its_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");

        for dir in [
            state.root().to_path_buf(),
            state.handoffs(),
            state.sockets(),
            state.logs(),
        ] {
            assert_eq!(
                mode_of(&dir),
                0o700,
                "{} is readable by other users",
                dir.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_files_are_private_to_their_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("state");
        create_private_dir_all(&root).expect("mkdir");

        let appended = root.join("appended.jsonl");
        drop(open_private_append(&appended).expect("append"));
        assert_eq!(mode_of(&appended), 0o600);

        let written = root.join("written.md");
        write_private(&written, "handoff").expect("write");
        assert_eq!(mode_of(&written), 0o600);
        assert_eq!(
            std::fs::read_to_string(&written).expect("read"),
            "handoff",
            "a private file is still a normal file"
        );
    }

    /// MED-2: `write_private` is atomic (temp sibling + rename), so a reader
    /// racing a rewrite never observes the zero-length truncation window a
    /// plain `std::fs::write` leaves. Hammer a rewrite in one thread while
    /// another reads and asserts the file, when present, is always one of the
    /// two whole contents -- never empty or partial.
    #[test]
    fn concurrent_reads_of_a_rewritten_file_never_see_a_partial_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("record.json");
        let a = "A".repeat(64 * 1024);
        let b = "B".repeat(64 * 1024);
        let (a_len, b_len) = (a.len(), b.len());
        // Seed one so the reader always finds a file to read.
        write_private(&path, &a).expect("seed");

        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            while !reader_stop.load(Ordering::Relaxed) {
                if let Ok(contents) = std::fs::read_to_string(&reader_path) {
                    assert!(
                        contents.len() == a_len || contents.len() == b_len,
                        "a reader must never observe a partial/empty file: saw {} bytes",
                        contents.len()
                    );
                }
            }
        });

        for i in 0..1000 {
            let contents = if i % 2 == 0 { &a } else { &b };
            write_private(&path, contents).expect("write");
        }
        stop.store(true, Ordering::Relaxed);
        reader
            .join()
            .expect("reader thread panicked on a partial read");
    }

    /// `write_shared` gets the same atomic temp-sibling-then-`rename`
    /// guarantee as `write_private` -- required for the memory shared
    /// scope's key-addressed files, where a concurrent reader/writer race on
    /// the very same path is the expected case, not an edge case. Same
    /// pattern as `concurrent_reads_of_a_rewritten_file_never_see_a_partial_
    /// write` above, using `write_shared` instead of `write_private`: a
    /// sequential write-then-read only proves two full writes each
    /// round-trip, not that a reader racing a rewrite never sees a partial
    /// one.
    #[test]
    fn concurrent_reads_of_a_rewritten_shared_file_never_see_a_partial_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("shared-entry.md");
        let a = "A".repeat(64 * 1024);
        let b = "B".repeat(64 * 1024);
        let (a_len, b_len) = (a.len(), b.len());
        write_shared(&path, &a).expect("seed");

        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            while !reader_stop.load(Ordering::Relaxed) {
                if let Ok(contents) = std::fs::read_to_string(&reader_path) {
                    assert!(
                        contents.len() == a_len || contents.len() == b_len,
                        "a reader must never observe a partial/empty shared file: saw {} bytes",
                        contents.len()
                    );
                }
            }
        });

        for i in 0..1000 {
            let contents = if i % 2 == 0 { &a } else { &b };
            write_shared(&path, contents).expect("write");
        }
        stop.store(true, Ordering::Relaxed);
        reader
            .join()
            .expect("reader thread panicked on a partial read");
    }

    /// Unlike `write_private`, `write_shared` must not force 0600: it writes
    /// ordinary repository content, so it should get whatever permissions a
    /// plain file write in the same directory would (whatever the process
    /// umask says), not the "machine-local secret" treatment.
    #[cfg(unix)]
    #[test]
    fn write_shared_uses_ordinary_permissions_not_write_privates_forced_0600() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let shared_path = tmp.path().join("shared.md");
        write_shared(&shared_path, "hello").expect("write_shared");
        let control_path = tmp.path().join("control.md");
        std::fs::write(&control_path, "hello").expect("control write");
        assert_eq!(
            mode_of(&shared_path),
            mode_of(&control_path),
            "write_shared must use the same ordinary, umask-respecting permissions as a plain file write, not write_private's forced 0600"
        );

        let private_path = tmp.path().join("private.md");
        write_private(&private_path, "hello").expect("write_private");
        assert_eq!(
            mode_of(&private_path),
            0o600,
            "sanity: write_private is still forced 0600 regardless of umask"
        );
    }

    /// `rename` replaces the directory entry itself rather than following
    /// it, so writing over a path that is currently a symlink must replace
    /// the link with a regular file -- never dereference it and write
    /// through to wherever it pointed. Load-bearing for the memory shared
    /// scope: a repo could otherwise commit `.zirv/memory/some-key.md` as a
    /// symlink to an arbitrary file on the machine.
    #[cfg(unix)]
    #[test]
    fn write_shared_replaces_a_symlinked_target_rather_than_writing_through_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "secret").expect("write outside");
        let linked = tmp.path().join("linked.md");
        std::os::unix::fs::symlink(&outside, &linked).expect("symlink");

        write_shared(&linked, "new content").expect("write over symlink");

        assert_eq!(
            std::fs::read_to_string(&outside).expect("read outside"),
            "secret",
            "the symlink target must never be written through"
        );
        assert!(
            !std::fs::symlink_metadata(&linked)
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the symlink itself must be replaced by a regular file"
        );
        assert_eq!(
            std::fs::read_to_string(&linked).expect("read linked"),
            "new content"
        );
    }

    #[test]
    fn the_usage_file_hangs_off_the_state_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(state.usage(), tmp.path().join("usage.json"));
    }

    #[test]
    fn the_per_provider_usage_file_hangs_off_the_state_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(
            state.usage_for("anthropic"),
            tmp.path().join("usage-anthropic.json")
        );
        assert_eq!(
            state.usage(),
            tmp.path().join("usage.json"),
            "the legacy global file is untouched"
        );
    }

    /// A provider slug names a file, so it must never be able to name a
    /// path: every separator and every `.` is folded to `-`, so the result
    /// always stays a single component inside the state root.
    #[test]
    fn a_provider_slug_can_never_escape_the_state_directory() {
        assert_eq!(provider_slug("anthropic"), "anthropic");
        assert_eq!(provider_slug("OpenAI"), "openai");
        assert_eq!(provider_slug("../../etc/passwd"), "------etc-passwd");
        assert_eq!(provider_slug("a b\\c"), "a-b-c");
        assert_eq!(provider_slug(""), "unknown");
        assert_eq!(provider_slug("..."), "---");

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let escaped = state.usage_for("../../../etc/passwd");
        assert_eq!(
            escaped.parent(),
            Some(tmp.path()),
            "still a direct child of the state root: {}",
            escaped.display()
        );
    }

    #[test]
    fn the_dash_dir_hangs_off_the_state_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(state.dash(), tmp.path().join("dash"));
    }
}
