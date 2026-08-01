// Consumed by verb entry points added in a later task of this plan; nothing
// calls this yet, so dead_code is silenced module-wide until then.
#![allow(dead_code)]

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
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl StateDir {
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

    /// First 8 hex characters of the session id keep the socket path short.
    pub fn socket_for(&self, session: &str) -> PathBuf {
        let short: String = session
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
        self.sockets().join(format!("{short}.sock"))
    }

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
