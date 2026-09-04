//! Issue #319: proof-required worktree reclaim, archive, startup GC, and the
//! `zirv ctx worktree` operator verbs. No worker worktree is ever removed
//! without affirmative proof that nothing is lost: untracked content is
//! archived, never destroyed, and a tree with any unpushed commit, any
//! dirty tracked file, or any cherry-unmatched commit is left in place with
//! a named reason.
//!
//! [`decide`] mirrors `rot.rs`'s own purity contract: it reads only what
//! [`probe`] already pulled from git as plain strings, never the
//! filesystem, the clock, or the environment, so identical probe outputs
//! give identical verdicts on every platform and every run. All I/O --
//! running git, copying files, appending the ownership record -- lives in
//! this module's other, non-pure functions, the same split `rot.rs`/
//! `score.rs` already draw.
//!
//! Ownership records live at `<state>/worktrees/<repo-slug>.jsonl`,
//! append-only and "materialized by path": [`read_records`] folds the file
//! down to the newest line per `path`, so a status change (`active` ->
//! `removed`/`inspection-failed`) is a fresh appended line, never an
//! in-place rewrite -- the same tolerant, corruption-resistant contract
//! `log::read_delegations` already gives `delegations.jsonl`.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::state::{StateDir, create_private_dir_all, now_secs, open_private_append};

/// One probe's own failure -- which probe (`"ahead"`, `"dirty"` or
/// `"cherry"`) and why, whether the underlying `git` command itself could
/// not be run/exited non-zero, or its output did not parse as expected.
/// [`decide`] treats every variant identically: any probe error refuses the
/// prune outright, naming this probe as the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    pub probe: &'static str,
    pub message: String,
}

/// A `git rev-list --count <base>..HEAD` (or `git cherry`) output that did
/// not parse as the plain-integer/line-prefixed shape those commands are
/// documented to produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Pure: parses `git rev-list --count <base>..HEAD`'s stdout -- a single
/// decimal integer, possibly with trailing whitespace/newline.
pub fn ahead_count(rev_list_output: &str) -> Result<u64, ParseError> {
    rev_list_output.trim().parse::<u64>().map_err(|e| {
        ParseError(format!(
            "could not parse `git rev-list --count` output {:?}: {e}",
            rev_list_output.trim()
        ))
    })
}

/// The tracked/untracked split of a `git status --porcelain` reading. A
/// worktree with `tracked: true` (any line not `?? ...`) is never a
/// candidate for `ArchiveThenRemove` -- only its untracked-only counterpart
/// is (see [`decide`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dirty {
    pub tracked: bool,
    pub untracked: Vec<PathBuf>,
}

/// Pure: classifies every non-empty `git status --porcelain` line. Porcelain
/// v1's untracked marker is exactly `"?? "` (two question marks, one
/// space) followed by the path; everything else -- staged, unstaged,
/// renamed, conflicted -- counts as tracked dirt, since none of those are
/// safe to silently discard.
pub fn is_dirty(porcelain_output: &str) -> Dirty {
    let mut dirty = Dirty::default();
    for line in porcelain_output.lines() {
        if line.is_empty() {
            continue;
        }
        match line.strip_prefix("?? ") {
            Some(rest) => dirty.untracked.push(PathBuf::from(rest)),
            None => dirty.tracked = true,
        }
    }
    dirty
}

/// Pure: counts `git cherry <base>`'s own `+` lines -- commits in `HEAD`
/// that are NOT equivalent to any commit already reachable from `<base>`.
/// A `-` line marks a commit already applied upstream (by rebase, squash or
/// cherry-pick) and is not counted: only a genuinely unmatched commit is
/// evidence of work that would be lost by removing the tree.
pub fn unmatched_commits(cherry_output: &str) -> u64 {
    cherry_output
        .lines()
        .filter(|line| line.starts_with('+'))
        .count() as u64
}

/// The three probes [`decide`] weighs, each independently fallible. Built by
/// [`probe`]; kept a plain struct (not a `Result<Probes, _>`) so a caller can
/// always see exactly which probe(s) succeeded even when one failed.
#[derive(Debug, Clone)]
pub struct Probes {
    pub ahead: Result<u64, ProbeFailure>,
    pub dirty: Result<Dirty, ProbeFailure>,
    pub unmatched: Result<u64, ProbeFailure>,
}

/// Why [`decide`] refused to remove a tree -- the first probe that failed or
/// came back non-clean, in probe order (`ahead`, then `dirty`, then
/// `cherry`), so a caller and an operator both see one unambiguous reason
/// rather than a partial summary of everything that might be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectionFailed {
    pub probe: &'static str,
    pub note: String,
}

/// [`decide`]'s verdict. `Remove` and `ArchiveThenRemove` are both
/// affirmative proof the tree carries nothing that would be lost;
/// `ArchiveThenRemove` additionally requires the caller to copy `Vec<PathBuf>`
/// (paths relative to the worktree root, exactly as `git status --porcelain`
/// reported them) somewhere durable before removing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneDecision {
    Remove,
    ArchiveThenRemove(Vec<PathBuf>),
    Keep(InspectionFailed),
}

/// Pure: the whole proof-required contract in one place. Any probe error, any
/// commit ahead of base, any tracked dirt, or any cherry-unmatched commit
/// refuses the prune -- named by the FIRST such condition in probe order,
/// never a merged summary. Only when all three probes came back clean AND
/// clean-of-tracked-dirt does untracked-only content route to
/// `ArchiveThenRemove`; a genuinely clean tree (no untracked content either)
/// routes to `Remove`.
pub fn decide(probes: &Probes) -> PruneDecision {
    let ahead = match &probes.ahead {
        Err(e) => {
            return PruneDecision::Keep(InspectionFailed {
                probe: e.probe,
                note: e.message.clone(),
            });
        }
        Ok(n) => *n,
    };
    if ahead > 0 {
        return PruneDecision::Keep(InspectionFailed {
            probe: "ahead",
            note: format!("{ahead} commit(s) ahead of base, not yet reachable from it"),
        });
    }
    let dirty = match &probes.dirty {
        Err(e) => {
            return PruneDecision::Keep(InspectionFailed {
                probe: e.probe,
                note: e.message.clone(),
            });
        }
        Ok(d) => d,
    };
    if dirty.tracked {
        return PruneDecision::Keep(InspectionFailed {
            probe: "dirty",
            note: "tracked changes present (staged, unstaged, renamed or conflicted)".to_string(),
        });
    }
    let unmatched = match &probes.unmatched {
        Err(e) => {
            return PruneDecision::Keep(InspectionFailed {
                probe: e.probe,
                note: e.message.clone(),
            });
        }
        Ok(n) => *n,
    };
    if unmatched > 0 {
        return PruneDecision::Keep(InspectionFailed {
            probe: "cherry",
            note: format!("{unmatched} commit(s) not equivalent to anything upstream of base"),
        });
    }
    if !dirty.untracked.is_empty() {
        return PruneDecision::ArchiveThenRemove(dirty.untracked.clone());
    }
    PruneDecision::Remove
}

/// Runs one git subcommand inside `path`, scrubbed of the ambient
/// `GIT_*`/worktree env vars the same way every other worktree-touching git
/// invocation in `agent.rs` already is, so a caller's own inherited
/// repository context (if this process happens to be running inside one)
/// never leaks into a probe aimed at a different tree.
fn run_git(path: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// I/O: runs the three git commands [`decide`] needs, inside `path`, against
/// the recorded `base_commit`. Every probe's own git command is scoped to
/// `path` alone (a worktree answers these questions about itself); there is
/// no separate `repo` argument to thread through only to leave unused.
pub fn probe(path: &Path, base_commit: &str) -> Probes {
    let ahead = run_git(
        path,
        &["rev-list", "--count", &format!("{base_commit}..HEAD")],
    )
    .map_err(|message| ProbeFailure {
        probe: "ahead",
        message,
    })
    .and_then(|out| {
        ahead_count(&out).map_err(|e| ProbeFailure {
            probe: "ahead",
            message: e.0,
        })
    });
    let dirty = run_git(path, &["status", "--porcelain"])
        .map(|out| is_dirty(&out))
        .map_err(|message| ProbeFailure {
            probe: "dirty",
            message,
        });
    let unmatched = run_git(path, &["cherry", base_commit])
        .map(|out| unmatched_commits(&out))
        .map_err(|message| ProbeFailure {
            probe: "cherry",
            message,
        });
    Probes {
        ahead,
        dirty,
        unmatched,
    }
}

/// Recursively copies `src` (a file or a whole untracked directory --
/// `git status --porcelain` reports an entirely-untracked directory as one
/// `?? dir/` line, not one line per file inside it) to `dst`, creating
/// parent directories as needed. Symlinks are followed and copied as
/// regular files/directories: this is an archive of content, not a
/// filesystem-faithful backup, and a dangling or cyclic symlink is exactly
/// the kind of thing an archive step must not choke on.
fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_path(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// Copies every path in `untracked` (relative to `path`, exactly as
/// `git status --porcelain` reported them) into a fresh directory under
/// `state.worktree_archive()`, and returns that directory only once EVERY
/// copy has succeeded. A failure partway through leaves whatever was already
/// copied on disk (harmless -- it is a copy, the originals are untouched)
/// and returns `Err`; the caller must treat that as `InspectionFailed`, not
/// remove anything.
pub fn archive_untracked(
    state: &StateDir,
    path: &Path,
    untracked: &[PathBuf],
) -> Result<PathBuf, String> {
    let slug = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("worktree");
    let dest = state
        .worktree_archive()
        .join(format!("{slug}-{}", now_secs()));
    create_private_dir_all(&dest)
        .map_err(|e| format!("could not create archive directory {}: {e}", dest.display()))?;
    for rel in untracked {
        let src = path.join(rel);
        let dst = dest.join(rel);
        copy_path(&src, &dst).map_err(|e| format!("could not archive {}: {e}", src.display()))?;
    }
    Ok(dest)
}

/// One line of a `<state>/worktrees/<repo-slug>.jsonl` file. `path` is the
/// canonicalized worktree path (matching what [`super::agent::
/// validate_workdir`] returns), rendered as a string for JSON -- comparisons
/// are exact-string, so every writer/reader of a given path must reach it
/// through the same canonicalization, which they do (`allocate_worktree`'s
/// own `validate_workdir` call is the sole minting point).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub path: String,
    pub branch: String,
    pub base_commit: String,
    #[serde(default)]
    pub owner_session: Option<String>,
    #[serde(default)]
    pub owner_pid: Option<u32>,
    pub created_at: u64,
    pub status: WorktreeStatus,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeStatus {
    Active,
    InspectionFailed,
    Removed,
}

impl std::fmt::Display for WorktreeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Active => "active",
            Self::InspectionFailed => "inspection-failed",
            Self::Removed => "removed",
        })
    }
}

fn record_path(state: &StateDir, repo_slug: &str) -> PathBuf {
    state.worktrees().join(format!("{repo_slug}.jsonl"))
}

/// Appends one ownership-record line. Append-only by design (see this
/// module's own doc comment) -- callers never rewrite or truncate this file,
/// only [`read_records`]/[`latest_for_path`] fold it down to current state.
pub fn append_record(state: &StateDir, repo_slug: &str, record: &WorktreeRecord) -> CtxResult<()> {
    create_private_dir_all(&state.worktrees())?;
    let mut file = open_private_append(&record_path(state, repo_slug))?;
    writeln!(file, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

/// Reads every parseable line of `<repo-slug>.jsonl`, folded down to the
/// newest record per `path` (later lines supersede earlier ones for the
/// same path -- the "materialized by path" contract). A line that fails to
/// parse is skipped, matching `log::read_delegations`'s own tolerance: one
/// malformed row must never fail the whole read. Returned oldest-`created_at`
/// first.
pub fn read_records(state: &StateDir, repo_slug: &str) -> Vec<WorktreeRecord> {
    let Ok(contents) = std::fs::read_to_string(record_path(state, repo_slug)) else {
        return Vec::new();
    };
    let mut by_path: std::collections::BTreeMap<String, WorktreeRecord> =
        std::collections::BTreeMap::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<WorktreeRecord>(line) {
            by_path.insert(record.path.clone(), record);
        }
    }
    let mut records: Vec<_> = by_path.into_values().collect();
    records.sort_by_key(|r| r.created_at);
    records
}

/// The current record for exactly `path`, if this repo has ever allocated a
/// worktree there.
pub fn latest_for_path(state: &StateDir, repo_slug: &str, path: &Path) -> Option<WorktreeRecord> {
    let target = path.to_string_lossy().to_string();
    read_records(state, repo_slug)
        .into_iter()
        .find(|r| r.path == target)
}

/// Appends a status-changing line for `path`'s existing record, carrying
/// forward every other field unchanged. A no-op (never fabricates a record)
/// when `path` has none -- there is nothing honest to update.
pub fn update_status(
    state: &StateDir,
    repo_slug: &str,
    path: &Path,
    status: WorktreeStatus,
    note: Option<String>,
) -> CtxResult<()> {
    let Some(mut record) = latest_for_path(state, repo_slug, path) else {
        return Ok(());
    };
    record.status = status;
    record.note = note;
    append_record(state, repo_slug, &record)
}

/// The result of one [`prune_one`] attempt. `Kept` and `Failed` both leave
/// the tree exactly as it was; the only difference is WHY: `Kept` is
/// [`decide`]'s own considered refusal (or a failed archive, named `"archive"`),
/// `Failed` is a plain I/O failure running `git worktree remove` itself
/// after a decision that should have succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneOutcome {
    Removed,
    Archived(PathBuf),
    Kept(InspectionFailed),
    Failed(String),
}

/// `git -C repo worktree remove <path>`, the single place this module
/// actually deletes a worktree directory. `force` is `--force`'d ONLY by
/// [`prune_one`]'s `ArchiveThenRemove` arm, and only after every untracked
/// file has already been copied to the archive -- `git worktree remove`
/// otherwise refuses outright on leftover untracked content (exactly the
/// content `decide` already proved is safe to discard because it now lives
/// elsewhere). The `Remove` arm (a genuinely clean tree, nothing left to
/// force past) never sets it.
fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<(), String> {
    let mut command = std::process::Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(repo)
        .arg("worktree")
        .arg("remove");
    if force {
        command.arg("--force");
    }
    let output = command
        .arg(path)
        .output()
        .map_err(|e| format!("git worktree remove: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git worktree remove: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// The full proof-required prune of one worktree: [`probe`] against
/// `base_commit`, then [`decide`], archiving untracked-only content first
/// when required, removing only once every prerequisite has succeeded, and
/// always leaving the ownership record reflecting the outcome. Shared by
/// [`gc`], the `prune` verb (`run_prune`), and
/// [`super::agent::reclaim_worktree`] -- one code path decides whether ANY
/// caller may remove a tree, so none of them can quietly drift from the
/// others' rigor.
pub fn prune_one(
    state: &StateDir,
    repo: &Path,
    repo_slug: &str,
    path: &Path,
    base_commit: &str,
) -> PruneOutcome {
    let probes = probe(path, base_commit);
    let outcome = match decide(&probes) {
        PruneDecision::Remove => match remove_worktree(repo, path, false) {
            Ok(()) => PruneOutcome::Removed,
            Err(reason) => PruneOutcome::Failed(reason),
        },
        PruneDecision::ArchiveThenRemove(untracked) => {
            match archive_untracked(state, path, &untracked) {
                // `--force`: the untracked content `git worktree remove`
                // would otherwise refuse to discard has already been copied
                // to `dest`, above -- see `remove_worktree`'s own doc
                // comment.
                Ok(dest) => match remove_worktree(repo, path, true) {
                    Ok(()) => PruneOutcome::Archived(dest),
                    Err(reason) => PruneOutcome::Failed(reason),
                },
                Err(reason) => PruneOutcome::Kept(InspectionFailed {
                    probe: "archive",
                    note: reason,
                }),
            }
        }
        PruneDecision::Keep(reason) => PruneOutcome::Kept(reason),
    };
    let (status, note) = match &outcome {
        PruneOutcome::Removed | PruneOutcome::Archived(_) => (WorktreeStatus::Removed, None),
        PruneOutcome::Kept(reason) => (
            WorktreeStatus::InspectionFailed,
            Some(format!("{}: {}", reason.probe, reason.note)),
        ),
        PruneOutcome::Failed(reason) => (WorktreeStatus::InspectionFailed, Some(reason.clone())),
    };
    let _ = update_status(state, repo_slug, path, status, note);
    outcome
}

/// Startup GC (issue #319, design item 4): conservative on purpose. A
/// record is a candidate only when its `owner_pid` is known AND
/// `is_alive(pid)` says dead -- `owner_pid: None` (an unrecorded or
/// pre-#319 owner) is left alone, since there is nothing to disprove
/// liveness against, and a `status != Active` record (already removed or
/// already flagged) is left alone too. Every candidate still goes through
/// the exact same [`prune_one`] proof an explicit `zirv ctx worktree prune`
/// would -- GC never removes anything more cheaply than an operator could.
/// A tree whose directory is already gone from disk (removed by some other
/// means) is simply marked `removed` without running any probe -- there is
/// nothing left to inspect.
pub fn gc(
    state: &StateDir,
    repo: &Path,
    is_alive: &dyn Fn(u32) -> bool,
) -> Vec<(WorktreeRecord, PruneOutcome)> {
    let repo_slug = super::state::repo_slug(repo);
    let mut outcomes = Vec::new();
    for record in read_records(state, &repo_slug) {
        if record.status != WorktreeStatus::Active {
            continue;
        }
        let Some(pid) = record.owner_pid else {
            continue;
        };
        if is_alive(pid) {
            continue;
        }
        let path = PathBuf::from(&record.path);
        if !path.is_dir() {
            let _ = update_status(
                state,
                &repo_slug,
                &path,
                WorktreeStatus::Removed,
                Some("directory no longer exists".to_string()),
            );
            outcomes.push((record, PruneOutcome::Removed));
            continue;
        }
        let outcome = prune_one(state, repo, &repo_slug, &path, &record.base_commit);
        outcomes.push((record, outcome));
    }
    outcomes
}

/// Pure: the sorted intersection of two `git diff --name-only <base>..<branch>`
/// outputs -- design item 5's merge-collision hotspot. Advisory only: no
/// caller acts on this beyond printing it, since there is no automated
/// resolution for two branches editing the same file.
pub fn hotspot_files(a_diff_name_only: &str, b_diff_name_only: &str) -> Vec<String> {
    let b: std::collections::BTreeSet<&str> = b_diff_name_only
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    let mut hotspots: Vec<String> = a_diff_name_only
        .lines()
        .filter(|line| !line.is_empty() && b.contains(line))
        .map(|line| line.to_string())
        .collect();
    hotspots.sort();
    hotspots.dedup();
    hotspots
}

/// Finds `id_or_path`'s record: an exact stored-path match, a canonicalized-
/// path match (a caller may type a differently-spelled but identical path),
/// or a match against the branch name `allocate_worktree` minted (the
/// worktree directory's own short id).
fn resolve_record(state: &StateDir, repo_slug: &str, id_or_path: &str) -> Option<WorktreeRecord> {
    let records = read_records(state, repo_slug);
    if let Some(r) = records.iter().find(|r| r.path == id_or_path) {
        return Some(r.clone());
    }
    if let Ok(canon) = std::fs::canonicalize(id_or_path) {
        let canon = canon.to_string_lossy().to_string();
        if let Some(r) = records.iter().find(|r| r.path == canon) {
            return Some(r.clone());
        }
    }
    records.into_iter().find(|r| r.branch == id_or_path)
}

#[derive(Debug, clap::Args)]
pub struct WorktreeArgs {
    #[command(subcommand)]
    pub command: WorktreeVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum WorktreeVerb {
    /// List every worktree this repo has allocated: path, branch, base
    /// commit, owner and whether it is still alive, and status.
    List(ListArgs),
    /// Report one worktree's branch, ahead-of-base count and dirty state,
    /// plus any merge-collision hotspot with another still-active worktree
    /// touching the same file. Read-only: never removes or archives
    /// anything, and leaves the branch for the orchestrator.
    Finalize(FinalizeArgs),
    /// Prune one worktree (or every eligible one with `--all`): the same
    /// proof-required probe/decide/archive contract automatic reclaim uses,
    /// run on demand. Prints one line per tree naming the decision and, on
    /// a refusal, the probe that refused.
    Prune(PruneArgs),
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct FinalizeArgs {
    /// The worktree's branch name (its short id) or its path.
    pub id_or_path: String,
}

#[derive(Debug, clap::Args)]
pub struct PruneArgs {
    /// The worktree's branch name (its short id) or its path. Required
    /// unless `--all` is given.
    pub id_or_path: Option<String>,
    /// Attempt every recorded `active` worktree for this repo.
    #[arg(long, default_value_t = false)]
    pub all: bool,
}

/// One line of `zirv ctx worktree list`'s plain-text output.
fn describe_record<W: Write>(
    w: &mut W,
    record: &WorktreeRecord,
    is_alive: &dyn Fn(u32) -> bool,
) -> CtxResult<()> {
    let alive = match record.owner_pid {
        Some(pid) if is_alive(pid) => "alive",
        Some(_) => "dead",
        None => "unknown",
    };
    write!(
        w,
        "{} branch={} base={} owner_session={} owner_pid={} alive={alive} status={}",
        record.path,
        record.branch,
        record.base_commit,
        record.owner_session.as_deref().unwrap_or("-"),
        record
            .owner_pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string()),
        record.status,
    )?;
    if let Some(note) = &record.note {
        write!(w, " note=\"{note}\"")?;
    }
    writeln!(w)?;
    Ok(())
}

#[derive(Serialize)]
struct JsonRow<'a> {
    path: &'a str,
    branch: &'a str,
    base_commit: &'a str,
    owner_session: Option<&'a str>,
    owner_pid: Option<u32>,
    alive: Option<bool>,
    status: String,
    note: Option<&'a str>,
}

pub fn run_list<W: Write>(
    state: &StateDir,
    repo: &Path,
    w: &mut W,
    args: &ListArgs,
    is_alive: &dyn Fn(u32) -> bool,
) -> CtxResult<i32> {
    let repo_slug = super::state::repo_slug(repo);
    let records = read_records(state, &repo_slug);
    if args.json {
        let rows: Vec<JsonRow> = records
            .iter()
            .map(|r| JsonRow {
                path: &r.path,
                branch: &r.branch,
                base_commit: &r.base_commit,
                owner_session: r.owner_session.as_deref(),
                owner_pid: r.owner_pid,
                alive: r.owner_pid.map(is_alive),
                status: r.status.to_string(),
                note: r.note.as_deref(),
            })
            .collect();
        writeln!(w, "{}", serde_json::to_string(&rows)?)?;
        return Ok(0);
    }
    if records.is_empty() {
        writeln!(w, "no worktrees recorded for this repo")?;
        return Ok(0);
    }
    for record in &records {
        describe_record(w, record, is_alive)?;
    }
    Ok(0)
}

pub fn run_finalize<W: Write>(
    state: &StateDir,
    repo: &Path,
    w: &mut W,
    args: &FinalizeArgs,
) -> CtxResult<i32> {
    let repo_slug = super::state::repo_slug(repo);
    let Some(record) = resolve_record(state, &repo_slug, &args.id_or_path) else {
        writeln!(w, "no worktree matches '{}'", args.id_or_path)?;
        return Ok(1);
    };
    let path = PathBuf::from(&record.path);
    let probes = probe(&path, &record.base_commit);
    write!(w, "{} branch={}", record.path, record.branch)?;
    match &probes.ahead {
        Ok(n) => write!(w, " ahead={n}")?,
        Err(e) => write!(w, " ahead=unknown({})", e.message)?,
    }
    match &probes.dirty {
        Ok(d) => write!(
            w,
            " tracked_dirty={} untracked_count={}",
            d.tracked,
            d.untracked.len()
        )?,
        Err(e) => write!(w, " dirty=unknown({})", e.message)?,
    }
    writeln!(w, " (branch left for the orchestrator; nothing removed)")?;

    // Design item 5: an advisory-only merge-collision hotspot against every
    // OTHER still-active worktree this repo has recorded.
    for other in read_records(state, &repo_slug) {
        if other.path == record.path || other.status != WorktreeStatus::Active {
            continue;
        }
        let (Ok(a_diff), Ok(b_diff)) = (
            run_git(
                repo,
                &[
                    "diff",
                    "--name-only",
                    &format!("{}..{}", record.base_commit, record.branch),
                ],
            ),
            run_git(
                repo,
                &[
                    "diff",
                    "--name-only",
                    &format!("{}..{}", other.base_commit, other.branch),
                ],
            ),
        ) else {
            continue;
        };
        for file in hotspot_files(&a_diff, &b_diff) {
            writeln!(
                w,
                "hotspot: {file} (also touched by branch {})",
                other.branch
            )?;
        }
    }
    Ok(0)
}

pub fn run_prune<W: Write>(
    state: &StateDir,
    repo: &Path,
    w: &mut W,
    args: &PruneArgs,
) -> CtxResult<i32> {
    let repo_slug = super::state::repo_slug(repo);
    let targets: Vec<WorktreeRecord> = if args.all {
        read_records(state, &repo_slug)
            .into_iter()
            .filter(|r| r.status == WorktreeStatus::Active)
            .collect()
    } else {
        let Some(id) = &args.id_or_path else {
            writeln!(w, "worktree prune: pass an id/path, or --all")?;
            return Ok(2);
        };
        match resolve_record(state, &repo_slug, id) {
            Some(r) => vec![r],
            None => {
                writeln!(w, "no worktree matches '{id}'")?;
                return Ok(1);
            }
        }
    };
    if targets.is_empty() {
        writeln!(w, "no active worktrees to prune")?;
        return Ok(0);
    }
    let mut any_kept = false;
    for record in targets {
        let path = PathBuf::from(&record.path);
        let outcome = prune_one(state, repo, &repo_slug, &path, &record.base_commit);
        match &outcome {
            PruneOutcome::Removed => writeln!(w, "{}: removed (clean)", record.path)?,
            PruneOutcome::Archived(dest) => writeln!(
                w,
                "{}: untracked content archived to {}, then removed",
                record.path,
                dest.display()
            )?,
            PruneOutcome::Kept(reason) => {
                any_kept = true;
                writeln!(
                    w,
                    "{}: kept ({}: {})",
                    record.path, reason.probe, reason.note
                )?;
            }
            PruneOutcome::Failed(reason) => {
                any_kept = true;
                writeln!(w, "{}: kept ({reason})", record.path)?;
            }
        }
    }
    Ok(if any_kept { 1 } else { 0 })
}

pub fn run<W: Write>(args: &WorktreeArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    let state = StateDir::resolve(&env)?;
    let repo = std::env::current_dir()?;
    match &args.command {
        WorktreeVerb::List(a) => run_list(&state, &repo, w, a, &super::sessions::is_alive),
        WorktreeVerb::Finalize(a) => run_finalize(&state, &repo, w, a),
        WorktreeVerb::Prune(a) => run_prune(&state, &repo, w, a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn init_repo(dir: &Path) {
        assert!(run(dir, &["init", "-q"]));
        assert!(run(dir, &["config", "user.email", "test@example.com"]));
        assert!(run(dir, &["config", "user.name", "test"]));
    }

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) -> String {
        std::fs::write(dir.join(file), contents).expect("write");
        assert!(run(dir, &["add", file]));
        assert!(run(dir, &["commit", "-q", "-m", message]));
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // -- pure probe parsing --------------------------------------------

    #[test]
    fn ahead_count_parses_a_plain_integer_with_trailing_newline() {
        assert_eq!(ahead_count("0\n"), Ok(0));
        assert_eq!(ahead_count("3\n"), Ok(3));
    }

    #[test]
    fn ahead_count_rejects_unparseable_output() {
        assert!(ahead_count("not a number").is_err());
    }

    #[test]
    fn is_dirty_separates_tracked_from_untracked() {
        let dirty = is_dirty(" M src/lib.rs\n?? scratch.txt\n?? dir/\n");
        assert!(dirty.tracked);
        assert_eq!(
            dirty.untracked,
            vec![PathBuf::from("scratch.txt"), PathBuf::from("dir/")]
        );
    }

    #[test]
    fn is_dirty_reports_untracked_only_when_nothing_is_tracked() {
        let dirty = is_dirty("?? scratch.txt\n");
        assert!(!dirty.tracked);
        assert_eq!(dirty.untracked, vec![PathBuf::from("scratch.txt")]);
    }

    #[test]
    fn is_dirty_on_a_clean_tree_is_empty() {
        let dirty = is_dirty("");
        assert!(!dirty.tracked);
        assert!(dirty.untracked.is_empty());
    }

    #[test]
    fn unmatched_commits_counts_only_plus_lines() {
        let cherry = "+ aaaa111 not yet upstream\n- bbbb222 already applied\n+ cccc333 also new\n";
        assert_eq!(unmatched_commits(cherry), 2);
    }

    #[test]
    fn unmatched_commits_on_empty_output_is_zero() {
        assert_eq!(unmatched_commits(""), 0);
    }

    // -- pure decide ------------------------------------------------------

    fn clean_probes() -> Probes {
        Probes {
            ahead: Ok(0),
            dirty: Ok(Dirty::default()),
            unmatched: Ok(0),
        }
    }

    #[test]
    fn decide_removes_a_genuinely_clean_tree() {
        assert_eq!(decide(&clean_probes()), PruneDecision::Remove);
    }

    #[test]
    fn decide_keeps_a_tree_with_any_ahead_commit() {
        let probes = Probes {
            ahead: Ok(1),
            ..clean_probes()
        };
        match decide(&probes) {
            PruneDecision::Keep(reason) => assert_eq!(reason.probe, "ahead"),
            other => panic!("expected Keep, got {other:?}"),
        }
    }

    #[test]
    fn decide_keeps_a_tree_with_tracked_dirt() {
        let probes = Probes {
            dirty: Ok(Dirty {
                tracked: true,
                untracked: Vec::new(),
            }),
            ..clean_probes()
        };
        match decide(&probes) {
            PruneDecision::Keep(reason) => assert_eq!(reason.probe, "dirty"),
            other => panic!("expected Keep, got {other:?}"),
        }
    }

    #[test]
    fn decide_keeps_a_tree_with_any_cherry_unmatched_commit() {
        let probes = Probes {
            unmatched: Ok(1),
            ..clean_probes()
        };
        match decide(&probes) {
            PruneDecision::Keep(reason) => assert_eq!(reason.probe, "cherry"),
            other => panic!("expected Keep, got {other:?}"),
        }
    }

    #[test]
    fn decide_archives_untracked_only_content_instead_of_keeping() {
        let probes = Probes {
            dirty: Ok(Dirty {
                tracked: false,
                untracked: vec![PathBuf::from("scratch.txt")],
            }),
            ..clean_probes()
        };
        assert_eq!(
            decide(&probes),
            PruneDecision::ArchiveThenRemove(vec![PathBuf::from("scratch.txt")])
        );
    }

    #[test]
    fn decide_names_the_first_failing_probe_when_several_are_wrong() {
        // ahead > 0 AND tracked dirt AND unmatched > 0 -- `ahead` must win,
        // since it is checked first.
        let probes = Probes {
            ahead: Ok(2),
            dirty: Ok(Dirty {
                tracked: true,
                untracked: Vec::new(),
            }),
            unmatched: Ok(3),
        };
        match decide(&probes) {
            PruneDecision::Keep(reason) => assert_eq!(reason.probe, "ahead"),
            other => panic!("expected Keep(ahead), got {other:?}"),
        }
    }

    #[test]
    fn decide_keeps_on_any_probe_error_naming_that_probe() {
        let probes = Probes {
            ahead: Err(ProbeFailure {
                probe: "ahead",
                message: "git not found".to_string(),
            }),
            ..clean_probes()
        };
        match decide(&probes) {
            PruneDecision::Keep(reason) => assert_eq!(reason.probe, "ahead"),
            other => panic!("expected Keep, got {other:?}"),
        }
    }

    // -- probe() integration, one per probe --------------------------------

    #[test]
    fn probe_ahead_reports_commits_made_in_the_worktree_since_base() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        init_repo(&repo);
        let base = commit(&repo, "README.md", "hello\n", "initial");
        let worktree = tmp.path().join("wt");
        assert!(run(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "wt",
                worktree.to_str().unwrap(),
                &base
            ]
        ));
        commit(&worktree, "extra.txt", "new\n", "worker commit");

        let probes = probe(&worktree, &base);
        assert_eq!(probes.ahead, Ok(1));
    }

    #[test]
    fn probe_dirty_reports_untracked_content_in_the_worktree() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        init_repo(&repo);
        let base = commit(&repo, "README.md", "hello\n", "initial");
        let worktree = tmp.path().join("wt");
        assert!(run(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "wt",
                worktree.to_str().unwrap(),
                &base
            ]
        ));
        std::fs::write(worktree.join("scratch.txt"), "not committed\n").expect("write");

        let probes = probe(&worktree, &base);
        let dirty = probes.dirty.expect("status must succeed");
        assert!(!dirty.tracked);
        assert_eq!(dirty.untracked, vec![PathBuf::from("scratch.txt")]);
    }

    #[test]
    fn probe_cherry_reports_zero_for_a_commit_already_equivalent_upstream() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        init_repo(&repo);
        let base = commit(&repo, "README.md", "hello\n", "initial");

        let probes = probe(&repo, &base);
        assert_eq!(probes.unmatched, Ok(0));
    }

    #[test]
    fn probe_cherry_reports_a_commit_not_yet_equivalent_upstream() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        init_repo(&repo);
        let base = commit(&repo, "README.md", "hello\n", "initial");
        let worktree = tmp.path().join("wt");
        assert!(run(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "wt",
                worktree.to_str().unwrap(),
                &base
            ]
        ));
        commit(&worktree, "extra.txt", "new\n", "worker commit");

        let probes = probe(&worktree, &base);
        assert_eq!(probes.unmatched, Ok(1));
    }

    // -- archive_untracked --------------------------------------------------

    #[test]
    fn archive_untracked_copies_files_and_whole_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(worktree.join("subdir")).expect("mkdir");
        std::fs::write(worktree.join("scratch.txt"), "not committed\n").expect("write");
        std::fs::write(worktree.join("subdir/nested.txt"), "also not committed\n").expect("write");

        let dest = archive_untracked(
            &state,
            &worktree,
            &[PathBuf::from("scratch.txt"), PathBuf::from("subdir/")],
        )
        .expect("archive must succeed");

        assert_eq!(
            std::fs::read_to_string(dest.join("scratch.txt")).expect("read archived file"),
            "not committed\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("subdir/nested.txt"))
                .expect("read archived nested file"),
            "also not committed\n"
        );
        // The archive is a copy: the originals must still be there for the
        // caller to remove afterward.
        assert!(worktree.join("scratch.txt").exists());
    }

    #[test]
    fn archive_untracked_fails_without_touching_anything_when_a_source_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).expect("mkdir");

        let result = archive_untracked(&state, &worktree, &[PathBuf::from("missing.txt")]);
        assert!(result.is_err(), "archiving a nonexistent path must fail");
    }

    // -- ownership record: append-only, materialized by path ----------------

    fn sample_record(path: &Path, status: WorktreeStatus) -> WorktreeRecord {
        WorktreeRecord {
            path: path.to_string_lossy().to_string(),
            branch: "abcd1234".to_string(),
            base_commit: "deadbeef".to_string(),
            owner_session: Some("sess-1".to_string()),
            owner_pid: Some(4242),
            created_at: 1_700_000_000,
            status,
            note: None,
        }
    }

    #[test]
    fn append_record_then_read_records_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let path = tmp.path().join("wt-a");
        append_record(
            &state,
            "repo-slug",
            &sample_record(&path, WorktreeStatus::Active),
        )
        .expect("append");

        let records = read_records(&state, "repo-slug");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, WorktreeStatus::Active);
    }

    #[test]
    fn read_records_materializes_the_newest_line_per_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let path = tmp.path().join("wt-a");
        append_record(
            &state,
            "repo-slug",
            &sample_record(&path, WorktreeStatus::Active),
        )
        .expect("append active");
        update_status(&state, "repo-slug", &path, WorktreeStatus::Removed, None)
            .expect("append removed");

        let records = read_records(&state, "repo-slug");
        assert_eq!(records.len(), 1, "one path must fold to one record");
        assert_eq!(records[0].status, WorktreeStatus::Removed);
    }

    #[test]
    fn read_records_skips_a_corrupt_line_without_failing_the_whole_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let path = tmp.path().join("wt-a");
        append_record(
            &state,
            "repo-slug",
            &sample_record(&path, WorktreeStatus::Active),
        )
        .expect("append");
        {
            let mut file =
                super::super::state::open_private_append(&record_path(&state, "repo-slug"))
                    .expect("open for corrupt append");
            writeln!(file, "not json at all").expect("write corrupt line");
        }

        let records = read_records(&state, "repo-slug");
        assert_eq!(records.len(), 1, "the corrupt line must be skipped");
    }

    #[test]
    fn update_status_is_a_no_op_for_a_path_with_no_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let path = tmp.path().join("never-recorded");
        update_status(&state, "repo-slug", &path, WorktreeStatus::Removed, None)
            .expect("must not error");
        assert!(read_records(&state, "repo-slug").is_empty());
    }

    #[test]
    fn latest_for_path_finds_only_the_matching_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let a = tmp.path().join("wt-a");
        let b = tmp.path().join("wt-b");
        append_record(
            &state,
            "repo-slug",
            &sample_record(&a, WorktreeStatus::Active),
        )
        .expect("append a");
        append_record(
            &state,
            "repo-slug",
            &sample_record(&b, WorktreeStatus::Active),
        )
        .expect("append b");

        let found = latest_for_path(&state, "repo-slug", &a).expect("record for a");
        assert_eq!(found.path, a.to_string_lossy());
        assert!(latest_for_path(&state, "repo-slug", &tmp.path().join("wt-c")).is_none());
    }

    // -- prune_one / gc: end-to-end over a real repo + recorded worktree ----

    /// Sets up a real repo with one commit, allocates a linked worktree via
    /// the same `-b <branch> <path> <base>` shape `agent::allocate_worktree`
    /// uses, and records a matching ownership record -- everything
    /// `prune_one`/`gc` need, without depending on `agent.rs`.
    fn repo_with_recorded_worktree(
        root: &Path,
        state: &StateDir,
        owner_pid: Option<u32>,
    ) -> (PathBuf, PathBuf, String) {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        init_repo(&repo);
        let base = commit(&repo, "README.md", "hello\n", "initial");
        let worktree = root.join("wt");
        assert!(run(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "wt",
                worktree.to_str().unwrap(),
                &base
            ]
        ));
        let repo_slug = super::super::state::repo_slug(&repo);
        append_record(
            state,
            &repo_slug,
            &WorktreeRecord {
                path: worktree.to_string_lossy().to_string(),
                branch: "wt".to_string(),
                base_commit: base.clone(),
                owner_session: None,
                owner_pid,
                created_at: 1_700_000_000,
                status: WorktreeStatus::Active,
                note: None,
            },
        )
        .expect("append record");
        (repo, worktree, base)
    }

    #[test]
    fn prune_one_removes_a_clean_tree_and_marks_the_record_removed() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, base) = repo_with_recorded_worktree(tmp.path(), &state, None);
        let repo_slug = super::super::state::repo_slug(&repo);

        let outcome = prune_one(&state, &repo, &repo_slug, &worktree, &base);
        assert_eq!(outcome, PruneOutcome::Removed);
        assert!(!worktree.exists());
        let record = latest_for_path(&state, &repo_slug, &worktree).expect("record");
        assert_eq!(record.status, WorktreeStatus::Removed);
    }

    #[test]
    fn prune_one_archives_untracked_content_then_removes_it() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, base) = repo_with_recorded_worktree(tmp.path(), &state, None);
        let repo_slug = super::super::state::repo_slug(&repo);
        std::fs::write(worktree.join("scratch.txt"), "not committed\n").expect("write");

        let outcome = prune_one(&state, &repo, &repo_slug, &worktree, &base);
        match outcome {
            PruneOutcome::Archived(dest) => {
                assert_eq!(
                    std::fs::read_to_string(dest.join("scratch.txt")).expect("read archived"),
                    "not committed\n"
                );
            }
            other => panic!("expected Archived, got {other:?}"),
        }
        assert!(
            !worktree.exists(),
            "the worktree must be removed after archiving"
        );
        let record = latest_for_path(&state, &repo_slug, &worktree).expect("record");
        assert_eq!(record.status, WorktreeStatus::Removed);
    }

    #[test]
    fn prune_one_keeps_a_tree_with_an_unpushed_commit_and_marks_inspection_failed() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, base) = repo_with_recorded_worktree(tmp.path(), &state, None);
        let repo_slug = super::super::state::repo_slug(&repo);
        commit(&worktree, "extra.txt", "new\n", "worker commit");

        let outcome = prune_one(&state, &repo, &repo_slug, &worktree, &base);
        match outcome {
            PruneOutcome::Kept(reason) => assert_eq!(reason.probe, "ahead"),
            other => panic!("expected Kept(ahead), got {other:?}"),
        }
        assert!(
            worktree.exists(),
            "a tree with an unpushed commit must survive"
        );
        let record = latest_for_path(&state, &repo_slug, &worktree).expect("record");
        assert_eq!(record.status, WorktreeStatus::InspectionFailed);
    }

    #[test]
    fn prune_one_keeps_a_tree_with_a_tracked_dirty_file() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, base) = repo_with_recorded_worktree(tmp.path(), &state, None);
        let repo_slug = super::super::state::repo_slug(&repo);
        std::fs::write(worktree.join("README.md"), "changed, but not committed\n").expect("write");

        let outcome = prune_one(&state, &repo, &repo_slug, &worktree, &base);
        match outcome {
            PruneOutcome::Kept(reason) => assert_eq!(reason.probe, "dirty"),
            other => panic!("expected Kept(dirty), got {other:?}"),
        }
        assert!(worktree.exists());
    }

    #[test]
    fn gc_never_touches_a_tree_whose_owner_pid_is_alive() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, _base) = repo_with_recorded_worktree(tmp.path(), &state, Some(4242));

        let outcomes = gc(&state, &repo, &|pid| pid == 4242);
        assert!(outcomes.is_empty());
        assert!(
            worktree.exists(),
            "a live owner's tree must never be touched"
        );
    }

    #[test]
    fn gc_removes_a_clean_tree_whose_owner_pid_is_dead() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, _base) = repo_with_recorded_worktree(tmp.path(), &state, Some(4242));

        let outcomes = gc(&state, &repo, &|_pid| false);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, PruneOutcome::Removed);
        assert!(!worktree.exists());
    }

    #[test]
    fn gc_never_touches_a_tree_with_no_recorded_owner_pid() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, _base) = repo_with_recorded_worktree(tmp.path(), &state, None);

        let outcomes = gc(&state, &repo, &|_pid| false);
        assert!(
            outcomes.is_empty(),
            "an unrecorded owner must never be assumed dead"
        );
        assert!(worktree.exists());
    }

    // -- hotspot_files ------------------------------------------------------

    #[test]
    fn hotspot_files_intersects_two_diff_name_only_outputs() {
        let a = "src/a.rs\nsrc/shared.rs\n";
        let b = "src/shared.rs\nsrc/b.rs\n";
        assert_eq!(hotspot_files(a, b), vec!["src/shared.rs".to_string()]);
    }

    #[test]
    fn hotspot_files_is_empty_when_nothing_overlaps() {
        assert!(hotspot_files("a.rs\n", "b.rs\n").is_empty());
    }

    // -- CLI verbs ------------------------------------------------------

    #[test]
    fn run_list_reports_no_worktrees_for_an_empty_state_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let mut out = Vec::new();
        let code = run_list(&state, &repo, &mut out, &ListArgs { json: false }, &|_| {
            false
        })
        .expect("run_list");
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("no worktrees recorded"));
    }

    #[test]
    fn run_prune_removes_a_clean_recorded_worktree_by_branch_name() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, _base) = repo_with_recorded_worktree(tmp.path(), &state, None);

        let mut out = Vec::new();
        let code = run_prune(
            &state,
            &repo,
            &mut out,
            &PruneArgs {
                id_or_path: Some("wt".to_string()),
                all: false,
            },
        )
        .expect("run_prune");
        assert_eq!(code, 0);
        assert!(!worktree.exists());
        assert!(String::from_utf8_lossy(&out).contains("removed"));
    }

    #[test]
    fn run_prune_reports_kept_and_exits_nonzero_when_a_probe_refuses() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, _base) = repo_with_recorded_worktree(tmp.path(), &state, None);
        commit(&worktree, "extra.txt", "new\n", "worker commit");

        let mut out = Vec::new();
        let code = run_prune(
            &state,
            &repo,
            &mut out,
            &PruneArgs {
                id_or_path: Some("wt".to_string()),
                all: false,
            },
        )
        .expect("run_prune");
        assert_eq!(code, 1);
        assert!(worktree.exists());
        assert!(String::from_utf8_lossy(&out).contains("kept (ahead"));
    }

    #[test]
    fn run_finalize_reports_ahead_and_dirty_state_without_removing_anything() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree, _base) = repo_with_recorded_worktree(tmp.path(), &state, None);

        let mut out = Vec::new();
        let code = run_finalize(
            &state,
            &repo,
            &mut out,
            &FinalizeArgs {
                id_or_path: "wt".to_string(),
            },
        )
        .expect("run_finalize");
        assert_eq!(code, 0);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("ahead=0"));
        assert!(text.contains("left for the orchestrator"));
        assert!(worktree.exists(), "finalize must never remove anything");
    }

    /// Design item 5: `finalize` names a merge-collision hotspot when
    /// another still-active worktree's branch touches the same file this
    /// one does, relative to each branch's own recorded base.
    #[test]
    fn run_finalize_names_a_hotspot_shared_with_another_active_worktree() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let (repo, worktree_a, base) = repo_with_recorded_worktree(tmp.path(), &state, None);
        let repo_slug = super::super::state::repo_slug(&repo);

        // A second worktree, on a second branch from the same base, editing
        // the SAME file ("README.md") -- the hotspot.
        let worktree_b = tmp.path().join("wt-b");
        assert!(run(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "wt-b",
                worktree_b.to_str().unwrap(),
                &base
            ]
        ));
        append_record(
            &state,
            &repo_slug,
            &WorktreeRecord {
                path: worktree_b.to_string_lossy().to_string(),
                branch: "wt-b".to_string(),
                base_commit: base.clone(),
                owner_session: None,
                owner_pid: None,
                created_at: 1_700_000_001,
                status: WorktreeStatus::Active,
                note: None,
            },
        )
        .expect("append record for wt-b");
        commit(&worktree_a, "README.md", "changed by a\n", "a edits README");
        commit(&worktree_b, "README.md", "changed by b\n", "b edits README");

        let mut out = Vec::new();
        run_finalize(
            &state,
            &repo,
            &mut out,
            &FinalizeArgs {
                id_or_path: "wt".to_string(),
            },
        )
        .expect("run_finalize");
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("hotspot: README.md"),
            "expected a hotspot line, got: {text}"
        );
    }
}
