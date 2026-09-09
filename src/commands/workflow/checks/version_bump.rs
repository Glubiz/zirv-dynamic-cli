//! ZCHK-VERSION-BUMP: when the diff against the base branch touches a path
//! that changes the shipped binary (`src/`, `Cargo.toml`, `Cargo.lock`,
//! `build.rs`), `Cargo.toml`'s `[package] version` must be strictly above
//! the base branch's (via `git merge-base`), and `Cargo.lock`'s own `zirv`
//! package entry must agree with `Cargo.toml`. A diff limited to everything
//! else (README/docs/scripts/.github/tests-fixtures/.zirv/*.md) needs no
//! bump at all. This isn't a duplicate-tag guard -- `.github/workflows/
//! cd.yaml`'s release step is idempotent on an already-published version
//! (it prints "already published; nothing to do" and skips the Homebrew/
//! Chocolatey jobs), so an unbumped merge just deploys nothing. A bump is
//! what makes a shipped-code change actually reach users. CI's
//! `version-bump` job (`.github/workflows/ci.yaml`) already enforces the
//! same path-filtered rule in bash after a push; this reimplements it so it
//! also runs locally, before a PR is even opened, via `zirv verify`/`zirv
//! verify --builtin`. The diff is taken against the working tree (`git diff
//! --name-only <base>`, no `HEAD` on the right side), matching how this
//! check is actually invoked: `zirv verify --builtin` runs pre-commit, so
//! uncommitted changes are the honest picture of what the PR will contain.
//! The changed-path set also includes untracked `src/` files (`git ls-files
//! --others --exclude-standard`, since a brand-new unstaged file is invisible
//! to `git diff` entirely) and uses `--no-renames` (since a file renamed OUT
//! of a shipped-code path otherwise shows only under its destination).

use std::cmp::Ordering;
use std::path::Path;
use std::process::Command;

use super::BuiltinCheckResult;

pub const ID: &str = "ZCHK-VERSION-BUMP";
const PROVES: &str = "when the diff against the base branch touches a shipped-code path (src/, Cargo.toml, \
     Cargo.lock, build.rs), Cargo.toml's [package] version is strictly above the base branch's, \
     and Cargo.lock's own zirv entry agrees with it";
const FIX: &str = "bump [package] version in Cargo.toml above the base branch's before opening \
     or updating the PR -- a version bump is what makes CD actually publish a shipped-code \
     change (CD is idempotent and deploys nothing on an unbumped, already-published version); \
     run `cargo build`/`cargo check` once afterward so Cargo.lock's own zirv entry picks up the \
     new version";
const ORIGIN: &str = "operator decision 2026-09-09 (Development/Decision Log.md): a README/docs/CI-only PR \
     should not be deployed, and CD's idempotent release step means it never was anyway; also \
     enforced in CI by .github/workflows/ci.yaml's version-bump job";

/// Paths whose change means the diff touches the shipped binary and
/// therefore needs a version bump. Anything else -- README.md, docs/**,
/// scripts/**, .github/**, tests/fixtures/**, .zirv/**, other *.md -- ships
/// nothing, so a version bump buys nothing for it.
fn touches_shipped_code(changed_paths: &str) -> bool {
    changed_paths.lines().any(|path| {
        path.starts_with("src/")
            || path == "Cargo.toml"
            || path == "Cargo.lock"
            || path == "build.rs"
    })
}

/// `git diff --name-only <base>` against the working tree (deliberately not
/// `<base> HEAD`): `zirv verify --builtin` runs pre-commit, so uncommitted
/// changes are part of what will actually land in the PR, and leaving them
/// out would let a shipped-code edit slip past this check unbumped simply
/// because it hadn't been committed yet. `--no-renames`: without it, a file
/// renamed OUT of a shipped-code path (e.g. `src/main.rs` -> `docs/moved.rs`)
/// is printed only under its destination, so the source path -- the one
/// that actually matters to `touches_shipped_code` -- never shows up at all;
/// `--no-renames` makes git report the pair as a plain deletion + addition
/// instead. Also folds in `git ls-files --others --exclude-standard`: a
/// brand-new file that was never `git add`ed is invisible to `git diff`
/// entirely (there is nothing yet to diff it against), so an unstaged new
/// `src/` file would otherwise slip past this check unbumped.
fn changed_paths_since(repo: &Path, base: &str) -> Result<String, String> {
    let mut diff = run_git_lines(repo, &["diff", "--name-only", "--no-renames", base])
        .map_err(|err| format!("cannot run git diff --name-only {base}: {err}"))?;
    let untracked = run_git_lines(repo, &["ls-files", "--others", "--exclude-standard"])
        .map_err(|err| format!("cannot run git ls-files --others --exclude-standard: {err}"))?;
    diff.push_str(&untracked);
    Ok(diff)
}

/// Runs `git -C <repo> <args>`, returning stdout as text on success.
fn run_git_lines(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|err| format!("{err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn run(repo: &Path) -> BuiltinCheckResult {
    // What this check actually guards is zirv's OWN release pipeline (every
    // merge to main publishes a release, and CD fails on a duplicate tag).
    // Another crate's versioning policy is none of its business -- and a
    // repository with no manifest at all is not this crate either.
    if !super::is_zirv_repo(repo) {
        return BuiltinCheckResult::not_applicable(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            super::not_the_zirv_repo(repo),
        );
    }
    let manifest_path = repo.join("Cargo.toml");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!("cannot read {}: {err}", manifest_path.display()),
            );
        }
    };
    let head_version = match parse_package_field(&manifest, "version") {
        Some(version) => version,
        None => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!(
                    "{} has no readable [package] version",
                    manifest_path.display()
                ),
            );
        }
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

    let changed_paths = match changed_paths_since(repo, &base) {
        Ok(paths) => paths,
        Err(reason) => return BuiltinCheckResult::inconclusive(ID, PROVES, FIX, ORIGIN, reason),
    };

    // No shipped-code path changed and the version is unchanged from base:
    // nothing here would actually ship differently, so a bump buys nothing.
    // A LOWER version still falls through to the comparison below and
    // fails regardless of what changed -- that's never a valid state.
    if !touches_shipped_code(&changed_paths) && head_version == base_version {
        return BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "no shipped-code paths changed since base ({base}); version {head_version} \
                 unchanged is fine"
            ),
        );
    }

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
    parse_package_field(&text, "version")
        .ok_or_else(|| format!("{rev}:Cargo.toml has no readable [package] version"))
}

pub(super) fn parse_package_field(text: &str, field: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(text).ok()?;
    value
        .get("package")?
        .get(field)?
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
/// versions never use, OR when the two sides have a different number of
/// components -- callers treat that as `Inconclusive`, never as a guessed
/// ordering. I-8: without the arity check, `Vec<u64>::cmp` also orders on
/// LENGTH once every shared component is equal, so `"3.22"` (missing the
/// patch component) compares as strictly *less than* `"3.22.0"` -- and, in
/// the opposite and more dangerous direction, `"3.22.0"` as strictly
/// *greater than* `"3.22"`, letting a Cargo.toml edit that changed nothing
/// about the actual version pass as a valid bump. This repo's own versions
/// are always exactly three components (`X.Y.Z`); a mismatched count on
/// either side is a shape neither `head_version` nor `base_version` should
/// ever actually take, so it is reported precisely rather than compared
/// numerically as if trailing components defaulted to zero.
fn compare_dotted_versions(left: &str, right: &str) -> Option<Ordering> {
    let parse = |raw: &str| -> Option<Vec<u64>> {
        raw.split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    };
    let left = parse(left)?;
    let right = parse(right)?;
    if left.len() != right.len() {
        return None;
    }
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

    /// This check guards zirv's own release pipeline; another crate (or a
    /// repository with no Cargo.toml at all) has no such rule to break, and
    /// counting it against `zirv verify` there made the command unable to
    /// exit 0 outside the zirv checkout.
    #[test]
    fn a_repository_that_is_not_the_zirv_crate_is_not_applicable() {
        let absent = tempdir().unwrap();
        assert_eq!(
            run(absent.path()).outcome,
            BuiltinOutcome::NotApplicable,
            "no Cargo.toml at all"
        );

        let other = tempdir().unwrap();
        std::fs::write(
            other.path().join("Cargo.toml"),
            "[package]\nname = \"somebody-elses-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let result = run(other.path());
        assert_eq!(result.outcome, BuiltinOutcome::NotApplicable, "{result:?}");
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

    /// A README-only diff changes nothing about the shipped binary, so an
    /// unbumped version passes -- CD is idempotent and would deploy nothing
    /// either way (operator decision 2026-09-09, Decision Log.md).
    #[test]
    fn a_docs_only_change_without_a_bump_passes() {
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
        assert_eq!(result.outcome, BuiltinOutcome::Pass, "{result:?}");
    }

    /// A change under `src/` ships different code, so it still needs a
    /// bump even though this repo's own manifest lives at the root --
    /// `touches_shipped_code` must catch it via the `src/` path, not just
    /// `Cargo.toml`/`Cargo.lock`/`build.rs`.
    #[test]
    fn a_src_change_without_a_bump_still_fails() {
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
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
        git(repo, &["add", "src/main.rs"]);
        git(repo, &["commit", "-q", "-m", "shipped-code change"]);

        let result = run(repo);
        assert_eq!(result.outcome, BuiltinOutcome::Fail, "{result:?}");
    }

    /// A LOWER version always fails. This diff isn't actually docs-only --
    /// changing the version at all necessarily edits `Cargo.toml`, which is
    /// itself one of the shipped-code paths, so `touches_shipped_code` is
    /// already true here regardless of the README change alongside it. The
    /// point of this test is just that a regressed version is never
    /// rescued by the "no bump needed" exemption, full stop.
    #[test]
    fn a_lower_version_always_fails() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let repo = tempdir().unwrap();
        let repo = repo.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "t@example.com"]);
        git(repo, &["config", "user.name", "t"]);
        write_cargo_toml(repo, "1.5.0");
        git(repo, &["add", "Cargo.toml"]);
        git(repo, &["commit", "-q", "-m", "base"]);

        git(repo, &["checkout", "-q", "-b", "feature"]);
        write_cargo_toml(repo, "1.4.0");
        std::fs::write(repo.join("README.md"), "unrelated change\n").unwrap();
        git(repo, &["add", "Cargo.toml", "README.md"]);
        git(
            repo,
            &["commit", "-q", "-m", "regressed version, docs only"],
        );

        let result = run(repo);
        assert_eq!(result.outcome, BuiltinOutcome::Fail, "{result:?}");
    }

    /// A brand-new, still-`git add`-less file under `src/` ships different
    /// code just as much as a tracked edit does -- `git diff --name-only`
    /// alone never shows it (it isn't a diff against anything yet), so
    /// `changed_paths_since` must also fold in `git ls-files --others
    /// --exclude-standard` or an unstaged new src file could slip past
    /// unbumped.
    #[test]
    fn an_untracked_new_src_file_without_a_bump_still_fails() {
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
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/new.rs"), "fn new() {}\n").unwrap();
        // Deliberately never `git add`ed: still untracked when `run` sees it.

        let result = run(repo);
        assert_eq!(result.outcome, BuiltinOutcome::Fail, "{result:?}");
    }

    /// `git diff --name-only` alone prints only the DESTINATION of a
    /// detected rename, so a file renamed OUT of `src/` (into a
    /// non-shipped-code path) would otherwise look like nothing under
    /// `src/` ever changed. `--no-renames` forces the source path to show
    /// up as its own deletion line instead.
    #[test]
    fn a_file_renamed_out_of_src_without_a_bump_still_fails() {
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
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
        git(repo, &["add", "Cargo.toml", "src/main.rs"]);
        git(repo, &["commit", "-q", "-m", "base"]);

        git(repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        git(repo, &["mv", "src/main.rs", "docs/moved.rs"]);
        git(repo, &["commit", "-q", "-m", "rename out of src"]);

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

    /// I-8: `"3.22"` and `"3.22.0"` are the same version with a missing
    /// patch component, not two comparable numeric versions -- a bare
    /// `Vec<u64>::cmp` orders on length once every shared component is
    /// equal, so `"3.22"` (head) would wrongly compare as *less than*
    /// `"3.22.0"` (base), and in the opposite, more dangerous direction a
    /// head of `"3.22.0"` against a base of `"3.22"` would wrongly compare
    /// as a valid bump even though nothing about the version actually
    /// changed. Mismatched component counts must be incomparable.
    #[test]
    fn mismatched_component_counts_are_not_comparable() {
        assert_eq!(compare_dotted_versions("3.22", "3.22.0"), None);
        assert_eq!(compare_dotted_versions("3.22.0", "3.22"), None);
    }

    /// The same scenario through the whole `run` check: a `Cargo.toml`
    /// missing its patch component against a base that has one is not a
    /// valid bump, and must be reported (Inconclusive, not a false Pass or
    /// a misleading Fail) rather than silently ordered as if the missing
    /// component defaulted to zero.
    #[test]
    fn a_component_count_mismatch_against_the_base_is_not_a_valid_bump() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let repo = tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "feature"]);
        git(repo.path(), &["config", "user.email", "t@example.com"]);
        git(repo.path(), &["config", "user.name", "t"]);
        write_cargo_toml(repo.path(), "3.22.0");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        git(repo.path(), &["branch", "-q", "main"]);

        write_cargo_toml(repo.path(), "3.22");
        std::fs::write(
            repo.path().join("Cargo.lock"),
            "[[package]]\nname = \"zirv\"\nversion = \"3.22\"\n",
        )
        .unwrap();

        let result = run(repo.path());
        assert_ne!(
            result.outcome,
            BuiltinOutcome::Pass,
            "a missing patch component must never look like a valid bump: {result:?}"
        );
    }
}
