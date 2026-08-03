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

/// Replaces every character outside `[A-Za-z0-9-]` with `-`, the same rule the
/// claude adapter uses for transcript directories.
pub fn repo_slug(path: &Path) -> String {
    path.to_string_lossy()
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

#[cfg(unix)]
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` applies only when the file is created, so writing over one that
    // already exists would leave whatever permissions it had -- an operator
    // who ran `touch report.md` first would get a world-readable report. Fail
    // rather than write private content somewhere that cannot be made private.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
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

    /// Per-transcript scoring checkpoints. The Stop hook is a fresh process on
    /// every turn, so the only place it can leave its parse position is a file.
    pub fn scoring(&self) -> PathBuf {
        self.0.join("scoring")
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
            path.to_string_lossy().ends_with("/s/00000000.sock"),
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

    #[test]
    fn the_usage_file_hangs_off_the_state_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(state.usage(), tmp.path().join("usage.json"));
    }
}
