//! ZCHK-VERSION-BUMP: `Cargo.toml`'s `[package] version` must be strictly
//! above the base branch's (via `git merge-base`), and `Cargo.lock`'s own
//! `zirv` package entry must agree with `Cargo.toml`. CI's `version-bump`
//! job (`.github/workflows/ci.yaml`) already enforces the first half in bash
//! after a push; this reimplements the same check so it also runs locally,
//! before a PR is even opened, via `zirv verify`/`zirv verify --builtin`.

use std::cmp::Ordering;
use std::path::Path;
use std::process::Command;

use super::BuiltinCheckResult;

pub const ID: &str = "ZCHK-VERSION-BUMP";
const PROVES: &str = "Cargo.toml's [package] version is strictly above the base branch's, and Cargo.lock's own \
     zirv entry agrees with it";
const FIX: &str = "bump [package] version in Cargo.toml above the base branch's before opening \
     or updating the PR (every merge to main publishes a release, and CD fails on a duplicate \
     tag); run `cargo build`/`cargo check` once afterward so Cargo.lock's own zirv entry picks \
     up the new version";
const ORIGIN: &str = "CD duplicate-tag failures -- reminded twice (Development/Decision Log.md, Known Issues.md); \
     also enforced in CI by .github/workflows/ci.yaml's version-bump job";

pub fn run(repo: &Path) -> BuiltinCheckResult {
    let head_version = match toml_package_version(&repo.join("Cargo.toml")) {
        Ok(version) => version,
        Err(reason) => return BuiltinCheckResult::inconclusive(ID, PROVES, FIX, ORIGIN, reason),
    };

    match lock_zirv_version(repo) {
        Ok(Some(lock_version)) if lock_version != head_version => {
            return BuiltinCheckResult::fail(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!(
                    "Cargo.toml version {head_version} does not match Cargo.lock's zirv entry \
                     {lock_version} -- run `cargo build`/`cargo check` to refresh the lockfile"
                ),
            );
        }
        Ok(_) => {}
        Err(reason) => return BuiltinCheckResult::inconclusive(ID, PROVES, FIX, ORIGIN, reason),
    }

    let base = match merge_base_ref(repo) {
        Ok(base) => base,
        Err(reason) => return BuiltinCheckResult::inconclusive(ID, PROVES, FIX, ORIGIN, reason),
    };

    let base_version = match toml_package_version_at(repo, &base) {
        Ok(version) => version,
        Err(reason) => return BuiltinCheckResult::inconclusive(ID, PROVES, FIX, ORIGIN, reason),
    };

    match compare_dotted_versions(&head_version, &base_version) {
        Some(Ordering::Greater) => BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!("HEAD {head_version} > base ({base}) {base_version}"),
        ),
        Some(_) => BuiltinCheckResult::fail(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!("HEAD {head_version} is not above base ({base}) {base_version}"),
        ),
        None => BuiltinCheckResult::inconclusive(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "could not compare versions '{head_version}' (HEAD) and '{base_version}' \
                 (base {base}) -- not both dotted-numeric"
            ),
        ),
    }
}

fn toml_package_version(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    parse_package_version(&text)
        .ok_or_else(|| format!("{} has no readable [package] version", path.display()))
}

fn toml_package_version_at(repo: &Path, rev: &str) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("show")
        .arg(format!("{rev}:Cargo.toml"))
        .output()
        .map_err(|err| format!("cannot run git show {rev}:Cargo.toml: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git show {rev}:Cargo.toml failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    parse_package_version(&text)
        .ok_or_else(|| format!("{rev}:Cargo.toml has no readable [package] version"))
}

fn parse_package_version(text: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(text).ok()?;
    value
        .get("package")?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

/// The `zirv` package's own version from `Cargo.lock`, or `Ok(None)` when
/// `Cargo.lock` is missing outright (a checkout that has never run `cargo
/// build`) -- distinct from a read/parse failure, which is `Err` and makes
/// the whole check `Inconclusive` rather than silently skipping the
/// cross-check.
fn lock_zirv_version(repo: &Path) -> Result<Option<String>, String> {
    let path = repo.join("Cargo.lock");
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| format!("cannot parse {}: {err}", path.display()))?;
    let packages = value
        .get("package")
        .and_then(|p| p.as_array())
        .ok_or_else(|| format!("{} has no [[package]] array", path.display()))?;
    for package in packages {
        if package.get("name").and_then(|n| n.as_str()) == Some("zirv") {
            return package
                .get("version")
                .and_then(|v| v.as_str())
                .map(|v| Some(v.to_string()))
                .ok_or_else(|| format!("{}'s zirv entry has no version", path.display()));
        }
    }
    Err(format!("{} has no zirv package entry", path.display()))
}

/// `git merge-base HEAD origin/main`, falling back to `git merge-base HEAD
/// main` when there is no `origin` remote tracking branch (a bare local
/// clone, a fork worked on without a fetch) -- same fallback order the
/// design calls for. `Err` (never a silent guess) when git itself is
/// unavailable, this isn't a git repository, or neither base exists.
fn merge_base_ref(repo: &Path) -> Result<String, String> {
    for base in ["origin/main", "main"] {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["merge-base", "HEAD", base])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !sha.is_empty() {
                    return Ok(sha);
                }
            }
            _ => {}
        }
    }
    Err(
        "no git, no git repository, or neither origin/main nor main is reachable to \
         merge-base against"
            .to_string(),
    )
}

/// Compares two dotted-numeric version strings (`"3.22.0"`) component-wise,
/// `None` when either side has a non-numeric component this repo's own
/// versions never use -- callers treat that as `Inconclusive`, never as a
/// guessed ordering.
fn compare_dotted_versions(left: &str, right: &str) -> Option<Ordering> {
    let parse = |raw: &str| -> Option<Vec<u64>> {
        raw.split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    };
    let left = parse(left)?;
    let right = parse(right)?;
    Some(left.cmp(&right))
}

#[cfg(test)]
mod tests {
    use super::super::BuiltinOutcome;
    use super::*;
    use tempfile::tempdir;

    fn write_cargo_toml(repo: &Path, version: &str) {
        std::fs::write(
            repo.join("Cargo.toml"),
            format!("[package]\nname = \"zirv\"\nversion = \"{version}\"\n"),
        )
        .unwrap();
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn no_git_repository_is_inconclusive_never_pass() {
        let repo = tempdir().unwrap();
        write_cargo_toml(repo.path(), "1.0.0");
        let result = run(repo.path());
        assert_eq!(result.outcome, BuiltinOutcome::Inconclusive, "{result:?}");
    }

    #[test]
    fn a_real_bump_above_main_passes() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let repo = tempdir().unwrap();
        let repo = repo.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "t@example.com"]);
        git(repo, &["config", "user.name", "t"]);
        write_cargo_toml(repo, "1.0.0");
        git(repo, &["add", "Cargo.toml"]);
        git(repo, &["commit", "-q", "-m", "base"]);

        git(repo, &["checkout", "-q", "-b", "feature"]);
        write_cargo_toml(repo, "1.1.0");
        git(repo, &["add", "Cargo.toml"]);
        git(repo, &["commit", "-q", "-m", "bump"]);

        let result = run(repo);
        assert_eq!(result.outcome, BuiltinOutcome::Pass, "{result:?}");
    }

    #[test]
    fn an_unchanged_version_against_main_fails() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let repo = tempdir().unwrap();
        let repo = repo.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "t@example.com"]);
        git(repo, &["config", "user.name", "t"]);
        write_cargo_toml(repo, "1.0.0");
        git(repo, &["add", "Cargo.toml"]);
        git(repo, &["commit", "-q", "-m", "base"]);

        git(repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("README.md"), "unrelated change\n").unwrap();
        git(repo, &["add", "README.md"]);
        git(repo, &["commit", "-q", "-m", "unrelated"]);

        let result = run(repo);
        assert_eq!(result.outcome, BuiltinOutcome::Fail, "{result:?}");
    }

    #[test]
    fn mismatched_lockfile_version_fails_before_the_git_comparison() {
        let repo = tempdir().unwrap();
        let repo = repo.path();
        write_cargo_toml(repo, "1.0.0");
        std::fs::write(
            repo.join("Cargo.lock"),
            "[[package]]\nname = \"zirv\"\nversion = \"0.9.0\"\n",
        )
        .unwrap();
        let result = run(repo);
        assert_eq!(result.outcome, BuiltinOutcome::Fail, "{result:?}");
        assert!(result.details.contains("0.9.0"), "{result:?}");
    }

    #[test]
    fn compare_dotted_versions_orders_numerically_not_lexicographically() {
        assert_eq!(
            compare_dotted_versions("3.9.0", "3.10.0"),
            Some(Ordering::Less)
        );
    }
}
