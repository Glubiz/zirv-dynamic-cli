use std::path::{Path, PathBuf};

/// Canonicalizes the longest existing prefix, then restores any missing tail.
/// A dangling symlink or inaccessible prefix is unresolvable, not a missing tail.
pub(crate) fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent()?;
            }
            Err(_) => return None,
        }
    }
    let tail = path.strip_prefix(existing).ok()?;
    std::fs::canonicalize(existing)
        .ok()
        .map(|root| root.join(tail))
}
