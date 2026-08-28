# Repo-Portable Memories (`zirv ctx remember --repo`/`recall`/`forget`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `zirv ctx remember`/`recall`/`forget` opt in to the shared, repository-owned memory bank (`.zirv/memory/`, already implemented for `zirv memory --shared`) via `--repo`, merge both banks with provenance labels on `recall`, and add a write-time secrets guard so a credential-shaped key or body is never silently committed through this path.

**Architecture:** The scope-generic store (`MemoryScope`, `upsert_scoped`/`list_scoped`/`forget_scoped`/`verify_scoped` in `src/commands/ctx/memory.rs`) already implements the shared bank end to end; this plan only widens the `zirv ctx` verb surface (`RememberArgs`/`RecallArgs`/`ForgetArgs` and their `run_*_with` functions) to reach the scope it already has, and adds one new write-time gate (`sensitive_shared_match`, called from `upsert_shared`) that every shared writer — explicit or automatic — passes through. The gate reuses `src/commands/workflow/review.rs`'s existing `detect_token_shape` regex rather than adding a new one.

**Tech Stack:** Rust 2024, clap derive, `regex` (already a workspace dependency, `Cargo.toml:26`), `serde`/`serde_json`, `cargo nextest`.

**Spec:** GitHub issue [#172](https://github.com/Glubiz/zirv-dynamic-cli/issues/172).

## Global Constraints

- From issue #172, verbatim: `zirv ctx remember --repo --key <k>` writes the entry to `.zirv/memory/<k>.md` (committed, follows the branch).
- From issue #172, verbatim: `zirv ctx recall` merges both banks, labeling each entry's provenance (local vs repo).
- From issue #172, verbatim: repo-sourced entries keep the existing trust posture (UNTRUSTED, information-only, capped, never able to loosen anything).
- From issue #172, verbatim: a secrets guard so keys/values like `staging-db-creds` are never silently committable via this path (deny-list or explicit warning/confirmation).
- From issue #172, verbatim: branch semantics are acceptable as-is (a memory written on a feature branch travels with the branch until merge) — no new git-awareness is required.
- Rust edition 2024 (`Cargo.toml:4`).
- Before claiming any task done, run all five: `cargo build`; `cargo nextest run --no-fail-fast`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`. On this Windows host, throttle to avoid the known 13900K instability: pass `--test-threads 8` to nextest/`cargo test` rather than the default. `--no-fail-fast` is mandatory — diff the sorted failure-NAME list against `main`'s own pre-existing failures (7 known `commands::ctx::wrap::tests` failures as of 2026-08-23; the count drifts, the name list is the baseline), never a raw count.
- Tests stay inline in `#[cfg(test)] mod tests` in the file they test — no new files under `tests/` for this feature (one existing fixture, `tests/fixtures/fake-model.sh`, gets a one-line data edit in Task 2).
- No new Cargo dependencies. `regex = "1"` is already a workspace dependency (`Cargo.toml:26`) and already used for exactly this kind of secret-shape detection in `src/commands/workflow/review.rs`'s `TOKEN_SHAPE_RE`/`detect_token_shape` — reuse it (widen its visibility to `pub(crate)`), do not write a second copy.
- Repo-layer config may only narrow, never widen (`REPO_FORBIDDEN` pattern in `src/commands/ctx/config.rs`). This feature adds no new `[memory]` config keys and must not change any existing key's `REPO_FORBIDDEN` status.
- Never commit or push directly to `main`/`master`. Work happens on a branch, e.g. `feat/172-repo-portable-memories`; open a PR when done.
- Every PR must bump `Cargo.toml`'s `version` above its base (currently `2.32.0`) or CD produces a duplicate release tag.
- No `Co-Authored-By` or `Generated with Claude Code` lines in commits or the PR description.
- Commit after each task goes green (new commit each time; never `--amend`).

---

## File Structure

- Modify: `src/commands/ctx/memory.rs` — `RememberArgs`/`ForgetArgs` gain `--repo` (Tasks 1, 4); `RememberArgs` gains `--allow-sensitive` (Task 2); `run_remember_with`/`run_forget_with` become scope-aware; `run_recall_with` merges both scopes with provenance labels (Task 3); new `sensitive_shared_term`/`sensitive_shared_match`/`upsert_shared_allow_sensitive`, `upsert_shared`'s signature and `write_durable`'s harvest loop change (Task 2); a handful of pre-existing shared-scope tests get de-flagged fixture data (Task 2).
- Modify: `src/commands/ctx/memory_cli.rs` — the `memory::RememberArgs` proxy `zirv memory remember`'s private arm builds gains the two new fields, always `false` (Tasks 1, 2).
- Modify: `src/commands/workflow/review.rs` — `detect_token_shape` becomes `pub(crate)` so `memory.rs` can call it (Task 2).
- Modify: `tests/fixtures/fake-model.sh` — one harvested-fact line renamed off a now-guarded phrase (Task 2).
- Modify: `docs/obsidian/Modules/Ctx Subsystem.md`, `docs/obsidian/Concepts/Untrusted Configuration.md` (Task 5).

---

### Task 1: `zirv ctx remember --repo` / `zirv ctx remember --verify --repo`

**Files:**
- Modify: `src/commands/ctx/memory.rs:1690-1842` (`RememberArgs`, `run_remember_with`, `run_remember`)
- Modify: `src/commands/ctx/memory.rs:2672-2680, 2716-2724, 2763-2771` (existing `RememberArgs { .. }` test literals)
- Modify: `src/commands/ctx/memory_cli.rs:554-563` (the `memory::RememberArgs` proxy `zirv memory remember`'s private arm builds)
- Test: `src/commands/ctx/memory.rs` (`#[cfg(test)] mod tests`, same file)

**Interfaces:**
- Consumes: `MemoryScope` (memory.rs:281), `upsert_scoped`/`verify_scoped` (memory.rs:805, 922), both already scope-generic and unchanged by this task.
- Produces: `RememberArgs.repo: bool` (the new `--repo` flag), consumed by Task 2 (which adds `allow_sensitive` alongside it) and by docs in Task 5.

- [ ] **Step 1: Add the `--repo` field to `RememberArgs`**

In `src/commands/ctx/memory.rs`, inside `pub struct RememberArgs` (starts at line 1690), insert a new field right after `verify`:

```rust
    /// With no text given, just refresh the existing entry's `Verified`
    /// stamp rather than requiring new text.
    #[arg(long, default_value_t = false)]
    pub verify: bool,
    /// Write (or, with `--verify`, refresh) this fact in the shared,
    /// repository-owned bank at `<repo>/.zirv/memory/<key>.md` instead of
    /// the private, machine-local one (issue #172). Committed with the
    /// repository, so it follows whichever branch it was written on until
    /// that branch merges -- see `zirv ctx recall`'s `[local]`/`[repo]`
    /// labels. Gated the same way `zirv memory remember --shared` already
    /// is: `cfg.memory.enabled && cfg.memory.shared_enabled`.
    #[arg(long, default_value_t = false)]
    pub repo: bool,
```

- [ ] **Step 2: Update `run_remember_with` to route on `args.repo`**

Replace the body of `run_remember_with` (memory.rs:1775-1836) with:

```rust
pub fn run_remember_with<W: Write>(
    args: &RememberArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    stdin: &mut dyn Read,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    if !cfg.memory.enabled {
        return Err(
            "zirv ctx remember: memory is disabled (memory.enabled = false); nothing was remembered"
                .into(),
        );
    }

    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let scope = if args.repo {
        MemoryScope::Shared
    } else {
        MemoryScope::Private
    };
    let bank_label = if args.repo { "repo" } else { "local" };

    match resolve_remember(args, stdin)? {
        RememberIntent::VerifyOnly => {
            if verify_scoped(scope, repo, &state, &slug, &args.key)? {
                writeln!(
                    w,
                    "zirv ctx remember: verified '{}' in the {bank_label} bank",
                    args.key
                )?;
                Ok(0)
            } else {
                Err(format!(
                    "zirv ctx remember: no entry for key '{}' in the {bank_label} bank",
                    args.key
                )
                .into())
            }
        }
        RememberIntent::Store(body) => {
            if body.is_empty() {
                return Err(
                    "zirv ctx remember: no text given; pass --text, --text-file, or pipe one on stdin"
                        .into(),
                );
            }
            let now = now_secs();
            let entry = Entry {
                key: args.key.clone(),
                written_by: identity_or_unknown(env, AGENT_ENV),
                written: now,
                verified: now,
                source: "explicit".to_string(),
                body,
                importance: args.importance.clone(),
                confidence: args.confidence.clone(),
                tags: args.tags.clone(),
                // Deliberately unwritable, unlike importance/confidence/tags
                // above: a path signal is inert until issue #44 wires it up
                // (see retrieval.rs's module doc), so no `--path` flag
                // exists to set it yet.
                paths: Vec::new(),
            };
            let path = upsert_scoped(scope, repo, &state, &slug, &cfg, &entry)
                .map_err(|e| format!("zirv ctx remember: {e}"))?;
            writeln!(
                w,
                "zirv ctx remember: stored '{}' in the {bank_label} bank at {}",
                args.key,
                path.display()
            )?;
            Ok(0)
        }
    }
}
```

Note `upsert_scoped(MemoryScope::Private, ...)` still delegates unchanged to `remember(state, slug, entry, cfg)` (memory.rs:805-817), so default (non-`--repo`) behavior is byte-for-byte the same as before except the two success/error message strings, which no existing test asserts on verbatim.

- [ ] **Step 3: Fix the three existing `RememberArgs { .. }` test literals in `memory.rs`**

Each of these (lines 2672, 2716, 2763) is missing the new field. Add `repo: false,` to each, e.g. the first becomes:

```rust
        let args = RememberArgs {
            key: "build-cmd".to_string(),
            text: Some("cargo build --release".to_string()),
            text_file: None,
            verify: false,
            repo: false,
            importance: None,
            confidence: None,
            tags: Vec::new(),
        };
```

Do the same for the literals at line 2716 (`remember_with_verify_and_no_text_only_refreshes_the_stamp`) and line 2763 (`memory_disabled_in_config_refuses_remember_and_reports_an_empty_recall_but_forget_still_works`).

- [ ] **Step 4: Fix the `memory::RememberArgs` proxy in `memory_cli.rs`**

In `src/commands/ctx/memory_cli.rs`, `run_remember_with`'s private arm (lines 554-562) builds a `memory::RememberArgs` directly. Add `repo: false,`:

```rust
        let ctx_args = memory::RememberArgs {
            key: args.key.clone(),
            text: Some(args.text.clone()),
            text_file: None,
            verify: false,
            repo: false,
            importance,
            confidence,
            tags: args.tags.clone(),
        };
        return memory::run_remember_with(&ctx_args, w, repo, env, stdin);
```

- [ ] **Step 5: Run the existing suite to confirm the struct-literal fix compiles and nothing broke**

Run: `cargo test --lib commands::ctx::memory:: --test-threads 8`
Expected: all pass (this is a compile-and-compat check before adding new behavior).

- [ ] **Step 6: Write the new `--repo` tests**

Add to `src/commands/ctx/memory.rs`'s `mod tests`:

```rust
    #[test]
    fn ctx_remember_repo_writes_to_dot_zirv_memory_and_recall_shows_it_labeled_repo() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        let args = RememberArgs {
            key: "onboarding-doc".to_string(),
            text: Some("see CONTRIBUTING.md for setup".to_string()),
            text_file: None,
            verify: false,
            repo: true,
            importance: None,
            confidence: None,
            tags: Vec::new(),
        };
        let mut out = Vec::new();
        run_remember_with(
            &args,
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("remember --repo");

        let path = repo.path().join(".zirv/memory/onboarding-doc.md");
        assert!(path.is_file(), "expected {}", path.display());
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("see CONTRIBUTING.md for setup")
        );
    }

    #[test]
    fn ctx_remember_repo_refuses_when_shared_enabled_is_false() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[
            (state::STATE_ENV, state_dir.to_str().expect("utf8")),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
        ]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let args = RememberArgs {
            key: "onboarding-doc".to_string(),
            text: Some("see CONTRIBUTING.md for setup".to_string()),
            text_file: None,
            verify: false,
            repo: true,
            importance: None,
            confidence: None,
            tags: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_remember_with(
            &args,
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect_err("shared_enabled = false must refuse the write");
        assert!(err.to_string().contains("shared_enabled"), "got {err}");
    }

    #[test]
    fn ctx_remember_verify_repo_refreshes_the_shared_entrys_stamp() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let cfg = CtxConfig::default();
        let slug = repo_slug(repo.path());

        let mut entry = sample("onboarding-doc", 1_700_000_000);
        entry.verified = 1_700_000_000;
        upsert_scoped(MemoryScope::Shared, repo.path(), &state, &slug, &cfg, &entry)
            .expect("seed shared");

        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = RememberArgs {
            key: "onboarding-doc".to_string(),
            text: None,
            text_file: None,
            verify: true,
            repo: true,
            importance: None,
            confidence: None,
            tags: Vec::new(),
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        run_remember_with(
            &args,
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("verify-only remember --repo");

        let refreshed = get_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            &slug,
            &cfg,
            "onboarding-doc",
        )
        .expect("get")
        .expect("present");
        assert!(refreshed.verified > 1_700_000_000, "verified was refreshed");
    }
```

- [ ] **Step 7: Run the new tests**

Run: `cargo test --lib commands::ctx::memory::tests::ctx_remember_repo -- --test-threads 8` and `cargo test --lib commands::ctx::memory::tests::ctx_remember_verify_repo -- --test-threads 8`
Expected: all pass.

- [ ] **Step 8: Run the full verify suite**

Run, in order: `cargo build`; `cargo nextest run --no-fail-fast --test-threads 8`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`.
Expected: no new failures beyond the known pre-existing `commands::ctx::wrap::tests` set on this host (compare the sorted failure-NAME list, not the count).

- [ ] **Step 9: Commit**

```bash
git add src/commands/ctx/memory.rs src/commands/ctx/memory_cli.rs
git commit -m "feat(ctx): add --repo to zirv ctx remember, writing/verifying in the shared bank"
```

---

### Task 2: Secrets guard on the shared write path, plus `--allow-sensitive`

**Files:**
- Modify: `src/commands/workflow/review.rs:465` (`detect_token_shape` visibility)
- Modify: `src/commands/ctx/memory.rs` (new guard functions; `upsert_shared`, `upsert_scoped`, `write_durable`; `RememberArgs`/`run_remember_with`; one test helper and a handful of pre-existing tests; new tests)
- Modify: `src/commands/ctx/memory_cli.rs:554-563` (the proxy from Task 1 gains `allow_sensitive: false`)
- Modify: `tests/fixtures/fake-model.sh:56`

**Interfaces:**
- Consumes: `crate::commands::workflow::review::detect_token_shape(&str) -> Option<&'static str>` (widened to `pub(crate)` in Step 1); `Entry` (memory.rs:24); `RememberArgs.repo` from Task 1.
- Produces: `memory::sensitive_shared_match(&Entry) -> Option<String>` and `memory::upsert_shared_allow_sensitive(repo, state, slug, cfg, entry) -> CtxResult<PathBuf>`, both `pub(crate)`/`pub` for later tasks and docs; `RememberArgs.allow_sensitive: bool`.

- [ ] **Step 1: Widen `detect_token_shape`'s visibility**

In `src/commands/workflow/review.rs`, change (around line 465):

```rust
fn detect_token_shape(text: &str) -> Option<&'static str> {
```

to:

```rust
pub(crate) fn detect_token_shape(text: &str) -> Option<&'static str> {
```

- [ ] **Step 2: De-flag `sample()`'s fixed body (pre-existing test collateral)**

`sample(key, written)` (memory.rs:2008-2021) always returns the body `"the staging DB creds live in 1Password."` regardless of `key`. Roughly twenty existing shared-scope tests call `sample(...)` unmodified and would start failing once the guard below exists, purely because of this incidental filler text (no test asserts on the literal string — confirmed by grep). Change the body (line 2015):

```rust
    fn sample(key: &str, written: u64) -> Entry {
        Entry {
            key: key.to_string(),
            written_by: "claude".to_string(),
            written,
            verified: written,
            source: "explicit".to_string(),
            body: "the build must pass locally before every release.".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        }
    }
```

- [ ] **Step 3: De-flag the one call site whose KEY (not `sample()`'s body) is itself credential-shaped**

In `upsert_scoped_shared_writes_two_unrelated_keys_to_two_different_files` (memory.rs:4181-4209), the key `"staging-db-creds"` itself contains "creds" independent of the body fix above. Change:

```rust
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("staging-db-creds", 2),
        )
        .expect("upsert b");

        assert!(dir.join("build-cmd.md").exists());
        assert!(dir.join("staging-db-creds.md").exists());
```

to:

```rust
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &sample("deploy-notes", 2),
        )
        .expect("upsert b");

        assert!(dir.join("build-cmd.md").exists());
        assert!(dir.join("deploy-notes.md").exists());
```

- [ ] **Step 4: De-flag the hand-inlined harvest test**

`a_harvest_updates_a_previously_harvested_key_but_never_an_explicit_one` (memory.rs:3449-3532) builds its own `Entry` literals rather than using `sample()`, and its accepted-candidate key is also `"staging-db-creds"`. Replace every occurrence of `"staging-db-creds"` and its body text within this one test with a guard-safe fixture. The explicit entry (around line 3455-3466):

```rust
        let explicit = Entry {
            key: "release-runbook".to_string(),
            written_by: "claude".to_string(),
            written: 1_000,
            verified: 1_000,
            source: "explicit".to_string(),
            body: "the release runbook lives in the internal wiki.".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
```

The accepted-candidates vec (around line 3491-3497):

```rust
        let accepted = vec![
            (
                "release-runbook".to_string(),
                "runbook moved to the new wiki space (inferred)".to_string(),
            ),
            ("build-cmd".to_string(), "cargo build --release".to_string()),
        ];
```

And both `get_scoped(..., "staging-db-creds")` calls (around lines 3502-3511) become `get_scoped(..., "release-runbook")`. The rest of the test (assertions on `still_explicit.body == explicit.body`, `refreshed.body`/`refreshed.source`) is unchanged since it compares against the `explicit`/`refreshed` variables, not literal strings.

- [ ] **Step 5: De-flag the harvest fixture script**

In `tests/fixtures/fake-model.sh`, line 56:

```sh
    printf 'staging-db-creds: staging DB creds live in 1Password under staging-db\n'
```

becomes:

```sh
    printf 'release-runbook: release runbook lives in the internal wiki under docs\n'
```

(No test hardcodes this key; the two `#[cfg(unix)]` harvest tests that consume it only assert `count > 0` and `source == "harvest"` on whatever comes back. These two tests are unix-only and will not run in this plan's Windows verification loop — note this as a residual to verify on Linux/Docker per the project's established practice for unix-only test changes.)

- [ ] **Step 6: Run the full memory test module to confirm Steps 2-5 left it green before any new behavior is added**

Run: `cargo test --lib commands::ctx::memory:: --test-threads 8`
Expected: all pass, identical to before this task (pure fixture renames, no behavior change yet).

- [ ] **Step 7: Write the new guard tests (red)**

Add to `src/commands/ctx/memory.rs`'s `mod tests`:

```rust
    #[test]
    fn upsert_scoped_shared_refuses_a_key_that_looks_credential_shaped() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let mut entry = sample("staging-db-creds", 1);
        entry.body = "see the ops runbook for details.".to_string();

        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &entry,
        )
        .expect_err("a key containing 'creds' must be refused");
        assert!(err.to_string().contains("credential"), "got {err}");
        assert!(
            !repo.path().join(".zirv/memory/staging-db-creds.md").exists(),
            "nothing must be written when the guard refuses"
        );
    }

    #[test]
    fn upsert_scoped_shared_refuses_a_body_containing_a_denylisted_term() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let mut entry = sample("deploy-notes", 1);
        entry.body = "the deploy token is rotated weekly.".to_string();

        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &entry,
        )
        .expect_err("a body containing 'token' must be refused");
        assert!(err.to_string().contains("credential"), "got {err}");
    }

    #[test]
    fn upsert_scoped_shared_refuses_a_body_containing_a_pem_private_key_block() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let mut entry = sample("deploy-key-notes", 1);
        entry.body = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAK\n-----END RSA PRIVATE KEY-----"
            .to_string();

        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &entry,
        )
        .expect_err("a PEM private-key block must be refused");
        assert!(err.to_string().contains("credential"), "got {err}");
    }

    #[test]
    fn upsert_scoped_shared_refuses_a_body_containing_an_aws_or_github_token_shape() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();

        let mut aws_entry = sample("infra-notes", 1);
        aws_entry.body = "rotate AKIAABCDEFGHIJKLMNOP if it leaks.".to_string();
        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &aws_entry,
        )
        .expect_err("an AWS-shaped access key id must be refused");
        assert!(err.to_string().contains("credential"), "got {err}");

        let mut gh_entry = sample("ci-notes", 2);
        gh_entry.body = "old value was ghp_abcdefghijklmnopqrstuvwxyz0123456789.".to_string();
        let err = upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &gh_entry,
        )
        .expect_err("a GitHub-shaped token must be refused");
        assert!(err.to_string().contains("credential"), "got {err}");
    }

    #[test]
    fn upsert_shared_allow_sensitive_bypasses_the_credential_guard() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let mut entry = sample("staging-db-creds", 1);
        entry.body = "see the ops runbook for details.".to_string();

        let path = upsert_shared_allow_sensitive(repo.path(), &state, "-irrelevant", &cfg, &entry)
            .expect("an explicit override must be allowed to write it anyway");
        assert!(path.is_file());
        assert!(path.ends_with("staging-db-creds.md"), "got {}", path.display());
    }

    #[test]
    fn upsert_scoped_private_never_consults_the_shared_credential_guard() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();
        let mut entry = sample("staging-db-creds", 1);
        entry.body = "the token rotates weekly.".to_string();

        let path = upsert_scoped(
            MemoryScope::Private,
            repo.path(),
            &state,
            "-irrelevant",
            &cfg,
            &entry,
        )
        .expect("the private scope never runs the shared-only credential guard");
        assert!(path.is_file());
    }

    #[test]
    fn write_durable_skips_a_credential_shaped_candidate_but_still_writes_the_rest() {
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(repo.path().join("state"));
        let cfg = CtxConfig::default();

        let accepted = vec![
            ("build-cmd".to_string(), "cargo build --release".to_string()),
            (
                "staging-db-creds".to_string(),
                "the staging DB creds live in 1Password.".to_string(),
            ),
            (
                "deploy-notes".to_string(),
                "deploy with zirv ctx exec".to_string(),
            ),
        ];
        let written = write_durable(repo.path(), &state, "-work-repo", &accepted, &cfg, 1_000)
            .expect("write_durable must not abort the whole batch");
        assert_eq!(
            written, 2,
            "the credential-shaped candidate is skipped, the other two land"
        );

        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-work-repo",
                &cfg,
                "build-cmd"
            )
            .expect("get")
            .is_some()
        );
        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-work-repo",
                &cfg,
                "deploy-notes"
            )
            .expect("get")
            .is_some()
        );
        assert!(
            get_scoped(
                MemoryScope::Shared,
                repo.path(),
                &state,
                "-work-repo",
                &cfg,
                "staging-db-creds"
            )
            .expect("get")
            .is_none(),
            "the flagged candidate must never be written"
        );
    }

    #[test]
    fn ctx_remember_repo_refuses_a_credential_shaped_key_unless_allow_sensitive_is_set() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());

        let args = RememberArgs {
            key: "staging-db-creds".to_string(),
            text: Some("see the ops runbook for details.".to_string()),
            text_file: None,
            verify: false,
            repo: true,
            allow_sensitive: false,
            importance: None,
            confidence: None,
            tags: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_remember_with(
            &args,
            &mut out,
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect_err("a credential-shaped key must be refused without --allow-sensitive");
        assert!(err.to_string().contains("credential"), "got {err}");

        let allowed = RememberArgs {
            allow_sensitive: true,
            ..args
        };
        let mut out2 = Vec::new();
        run_remember_with(
            &allowed,
            &mut out2,
            repo.path(),
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("--allow-sensitive must permit the write");
        assert!(
            repo.path()
                .join(".zirv/memory/staging-db-creds.md")
                .is_file()
        );
    }
```

- [ ] **Step 8: Run the new tests to verify they fail (compile error expected: `upsert_shared_allow_sensitive`/`allow_sensitive` don't exist yet)**

Run: `cargo test --lib commands::ctx::memory:: --test-threads 8`
Expected: FAIL to compile, naming the missing `allow_sensitive` field and `upsert_shared_allow_sensitive` function.

- [ ] **Step 9: Implement the guard and thread `allow_sensitive` through**

In `src/commands/ctx/memory.rs`, add just above `upsert_shared` (currently at line 752):

```rust
/// Case-insensitive substrings whose presence in a shared-scope key or body
/// mark the entry as probably holding a live credential rather than a
/// durable, safe-to-commit repository fact (issue #172). Deliberately blunt,
/// the same style `workflow::review::is_sensitive_name`'s own
/// `name.contains("credential")`/`name.contains("secret")` already use for a
/// sibling problem (screening an untracked file before a review package): a
/// false positive costs the writer a private `remember` or an explicit
/// `--allow-sensitive` retry; a false negative commits a secret to every
/// future clone of the repository.
const SENSITIVE_SHARED_TERMS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api-key",
    "api_key",
    "apikey",
    "credential",
    "creds",
    "private-key",
    "private_key",
];

fn sensitive_shared_term(haystack: &str) -> Option<&'static str> {
    let lower = haystack.to_ascii_lowercase();
    SENSITIVE_SHARED_TERMS
        .iter()
        .copied()
        .find(|term| lower.contains(term))
}

/// Whether `entry` looks credential-shaped rather than a durable,
/// safe-to-commit repository fact: a deny-list term in the key or body
/// (`sensitive_shared_term`), or a known token shape in the body
/// (`review::detect_token_shape` -- OpenAI/GitHub/Slack/AWS-style keys, a PEM
/// private-key block, a JWT -- reused rather than duplicated, since the
/// `regex` crate and these exact families are already a workspace
/// dependency). `None` means nothing matched. Pure (no I/O), so both
/// `upsert_shared` (a hard refusal) and `write_durable`'s harvest loop (a
/// skip-and-log, never an abort) can call it without either owning its own
/// copy of the rule.
fn sensitive_shared_match(entry: &Entry) -> Option<String> {
    if let Some(term) = sensitive_shared_term(&entry.key) {
        return Some(format!("the term '{term}' in its key"));
    }
    if let Some(term) = sensitive_shared_term(&entry.body) {
        return Some(format!("the term '{term}' in its body"));
    }
    crate::commands::workflow::review::detect_token_shape(&entry.body)
        .map(|family| format!("a {family} in its body"))
}
```

Then change `upsert_shared`'s signature and body (memory.rs:752-787). The guard check goes LAST, after the existing key/field/directory/collision checks and immediately before the write, so every pre-existing rejection (invalid key, embedded newline, symlinked directory, canonical-key collision) still reports its own original error text first, exactly as before:

```rust
fn upsert_shared(
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    entry: &Entry,
    allow_sensitive: bool,
) -> CtxResult<PathBuf> {
    if !MemoryScope::Shared.enabled(cfg) {
        let reason = MemoryScope::Shared.disabled_reason(cfg);
        return Err(format!("shared memory is disabled ({reason}); nothing was stored").into());
    }
    validate_shared_key(&entry.key)?;
    validate_shared_entry_fields(entry)?;
    let Some(dir) = MemoryScope::Shared.dir(repo, state, slug) else {
        return Err(
            "the shared memory directory is unsafe (a symlink) and cannot be written to".into(),
        );
    };
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}.md", entry.key));
    for (other_path, other) in read_entries(&dir)? {
        if other.key == entry.key && other_path != path {
            return Err(format!(
                "memory key '{}' is already claimed by {} (its canonical file is {}); refusing to create a duplicate",
                entry.key,
                other_path.display(),
                path.display(),
            )
            .into());
        }
    }

    if !allow_sensitive
        && let Some(matched) = sensitive_shared_match(entry)
    {
        return Err(format!(
            "memory key '{}' looks like it holds a credential ({matched}); the shared bank at .zirv/memory/ is committed to the repository and readable by anyone with checkout access. Store it in the private bank instead (drop --repo), or pass --allow-sensitive to store it in the shared bank anyway.",
            entry.key
        )
        .into());
    }

    super::state::write_shared(&path, &entry.to_markdown())?;
    Ok(path)
}

/// Explicit escape hatch for a human who has deliberately decided a shared
/// entry should be committed despite looking credential-shaped (`zirv ctx
/// remember --repo --allow-sensitive`, issue #172). Every other shared-scope
/// writer -- `upsert_scoped` (and so every automatic path through it:
/// `zirv memory remember --shared`, `zirv memory optimize --apply`'s
/// consolidation) -- always calls `upsert_shared` with `allow_sensitive =
/// false`, and `write_durable`'s harvest loop skips a flagged candidate
/// before ever reaching `upsert_scoped` for it (see below) -- so this bypass
/// exists on exactly one, explicitly-named, human-invoked path and nowhere
/// else.
pub fn upsert_shared_allow_sensitive(
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    entry: &Entry,
) -> CtxResult<PathBuf> {
    upsert_shared(repo, state, slug, cfg, entry, true)
}
```

Update `upsert_scoped`'s `Shared` arm (memory.rs:805-817) to pass the new parameter:

```rust
pub fn upsert_scoped(
    scope: MemoryScope,
    repo: &Path,
    state: &StateDir,
    slug: &str,
    cfg: &CtxConfig,
    entry: &Entry,
) -> CtxResult<PathBuf> {
    match scope {
        MemoryScope::Private => remember(state, slug, entry, cfg),
        MemoryScope::Shared => upsert_shared(repo, state, slug, cfg, entry, false),
    }
}
```

Update `write_durable`'s per-candidate loop (memory.rs:1557-1600) to skip-and-log a flagged candidate before it ever reaches `upsert_scoped`, so one bad candidate never aborts the rest of the harvest batch:

```rust
fn write_durable(
    repo: &Path,
    state: &StateDir,
    slug: &str,
    accepted: &[(String, String)],
    cfg: &CtxConfig,
    now: u64,
) -> CtxResult<usize> {
    let mut written = 0usize;
    for (key, body) in accepted {
        if let Some(existing) = get_scoped(MemoryScope::Shared, repo, state, slug, cfg, key)?
            && existing.source == "explicit"
        {
            let _ = super::log::append(
                state,
                &super::log::Decision {
                    ts: now,
                    session: "n/a",
                    verb: "memory",
                    verdict: "n/a",
                    score: 0,
                    action: "harvest-skipped",
                    detail: &format!("'{key}' is already an explicit entry"),
                },
            );
            continue;
        }
        let entry = Entry {
            key: key.clone(),
            written_by: "harvest".to_string(),
            written: now,
            verified: now,
            source: "harvest".to_string(),
            body: body.clone(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        if let Some(matched) = sensitive_shared_match(&entry) {
            let _ = super::log::append(
                state,
                &super::log::Decision {
                    ts: now,
                    session: "n/a",
                    verb: "memory",
                    verdict: "n/a",
                    score: 0,
                    action: "harvest-skipped",
                    detail: &format!(
                        "'{key}' looks credential-shaped ({matched}); refusing to harvest it into the shared bank"
                    ),
                },
            );
            continue;
        }
        upsert_scoped(MemoryScope::Shared, repo, state, slug, cfg, &entry)?;
        written += 1;
    }
    Ok(written)
}
```

Add `allow_sensitive` to `RememberArgs` (right after the `repo` field added in Task 1):

```rust
    /// Store into the shared bank even though its key or body looks
    /// credential-shaped (`memory::sensitive_shared_match`). Has no effect
    /// without `--repo`, and no effect on the private bank, which the guard
    /// never inspects at all.
    #[arg(long, default_value_t = false)]
    pub allow_sensitive: bool,
```

Update `run_remember_with`'s `Store` arm to honor it: replace the `upsert_scoped(...)` call from Task 1 with:

```rust
            let path = if scope == MemoryScope::Shared && args.allow_sensitive {
                upsert_shared_allow_sensitive(repo, &state, &slug, &cfg, &entry)
            } else {
                upsert_scoped(scope, repo, &state, &slug, &cfg, &entry)
            }
            .map_err(|e| format!("zirv ctx remember: {e}"))?;
```

- [ ] **Step 10: Add `allow_sensitive: false` to the four pre-existing `RememberArgs { .. }` literals**

The three test literals in `memory.rs` (lines 2672, 2716, 2763, already carrying `repo: false` from Task 1) and the `memory::RememberArgs` proxy in `memory_cli.rs` (lines 554-563, same) each need `allow_sensitive: false,` added alongside `repo: false,`.

- [ ] **Step 11: Run the new and existing tests to verify green**

Run: `cargo test --lib commands::ctx::memory:: --test-threads 8`
Expected: all pass, including every test added in Step 7 and every test fixed in Steps 3-5.

- [ ] **Step 12: Run the full verify suite**

Run, in order: `cargo build`; `cargo nextest run --no-fail-fast --test-threads 8`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`.
Expected: no new failures beyond the known pre-existing `commands::ctx::wrap::tests` set.

- [ ] **Step 13: Commit**

```bash
git add src/commands/ctx/memory.rs src/commands/ctx/memory_cli.rs src/commands/workflow/review.rs tests/fixtures/fake-model.sh
git commit -m "feat(ctx): refuse credential-shaped shared memory writes unless --allow-sensitive"
```

---

### Task 3: `zirv ctx recall` merges both banks with provenance labels

**Files:**
- Modify: `src/commands/ctx/memory.rs:1844-1888` (`run_recall_with`, plus a new `RecallEntry` struct just above it)
- Test: `src/commands/ctx/memory.rs` (`#[cfg(test)] mod tests`, same file)

**Interfaces:**
- Consumes: `list_scoped(scope, repo, state, slug, cfg) -> CtxResult<Vec<(PathBuf, Entry)>>` (memory.rs:419), unchanged; `MemoryScope` (memory.rs:281).
- Produces: no new public symbols — `run_recall_with`'s human and `--json` output both change shape (every line now carries a `[local]`/`[repo]` label or a `"scope"` JSON field).

- [ ] **Step 1: Write the failing tests**

Add to `src/commands/ctx/memory.rs`'s `mod tests`:

```rust
    #[test]
    fn ctx_recall_merges_both_banks_and_labels_each_entrys_provenance() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_dir = repo.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let cfg = CtxConfig::default();
        let slug = repo_slug(repo.path());

        let mut local = sample("local-fact", 1);
        local.body = "only ever lives on this machine".to_string();
        remember(&state, &slug, &local, &cfg).expect("remember private");

        let mut shared = sample("repo-fact", 2);
        shared.body = "travels with the branch".to_string();
        upsert_scoped(MemoryScope::Shared, repo.path(), &state, &slug, &cfg, &shared)
            .expect("upsert shared");

        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = RecallArgs {
            key: None,
            stale: None,
            json: false,
        };
        let mut out = Vec::new();
        run_recall_with(&args, &mut out, repo.path(), &|k| env.get(k).cloned()).expect("recall");
        let text = String::from_utf8(out).expect("utf8");

        let local_at = text.find("local-fact").expect("local entry present");
        let repo_at = text.find("repo-fact").expect("repo entry present");
        assert!(
            local_at < repo_at,
            "local entries are listed before repo ones: {text}"
        );
        assert!(text.contains("[local]"), "got {text}");
        assert!(text.contains("[repo --"), "got {text}");
    }

    #[test]
    fn ctx_recall_json_includes_a_scope_field_for_each_entry() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_dir = repo.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let cfg = CtxConfig::default();
        let slug = repo_slug(repo.path());

        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            &slug,
            &cfg,
            &sample("repo-fact", 1),
        )
        .expect("upsert shared");

        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = RecallArgs {
            key: None,
            stale: None,
            json: true,
        };
        let mut out = Vec::new();
        run_recall_with(&args, &mut out, repo.path(), &|k| env.get(k).cloned()).expect("recall");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\"scope\":\"repo\""), "got {text}");
    }

    #[test]
    fn ctx_recall_with_key_shows_the_local_entry_before_the_repo_entry_for_the_same_key() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_dir = repo.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let cfg = CtxConfig::default();
        let slug = repo_slug(repo.path());

        let mut local = sample("shared-key-name", 1);
        local.body = "local version of the fact".to_string();
        remember(&state, &slug, &local, &cfg).expect("remember private");

        let mut shared = sample("shared-key-name", 2);
        shared.body = "repo version of the fact".to_string();
        upsert_scoped(MemoryScope::Shared, repo.path(), &state, &slug, &cfg, &shared)
            .expect("upsert shared");

        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = RecallArgs {
            key: Some("shared-key-name".to_string()),
            stale: None,
            json: false,
        };
        let mut out = Vec::new();
        run_recall_with(&args, &mut out, repo.path(), &|k| env.get(k).cloned()).expect("recall");
        let text = String::from_utf8(out).expect("utf8");
        let local_at = text.find("local version").expect("local entry present");
        let repo_at = text.find("repo version").expect("repo entry present");
        assert!(local_at < repo_at, "local first: {text}");
    }
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test --lib commands::ctx::memory::tests::ctx_recall -- --test-threads 8`
Expected: FAIL — today's `run_recall_with` reads only the private bank, so `repo-fact`/`"repo version of the fact"` are never found and `[repo` never appears in the output.

- [ ] **Step 3: Implement the merge and labeling**

Add, just above `run_recall_with` (memory.rs:1844):

```rust
/// One recalled entry plus the bank it came from, for `zirv ctx recall
/// --json` (issue #172). `scope` is derived from which bank `run_recall_with`
/// actually read it from, never from the entry's own header fields -- a
/// shared entry's `Source`/`Written-By` are themselves attacker-supplied
/// repository content (see `MemoryScope::Shared`'s own doc comment), the same
/// rule `memory_cli.rs`'s own `ScopedEntry` already follows for `zirv memory
/// recall`.
#[derive(Serialize)]
struct RecallEntry<'a> {
    #[serde(flatten)]
    entry: &'a Entry,
    scope: &'static str,
}
```

Replace `run_recall_with`'s body (memory.rs:1844-1888):

```rust
pub fn run_recall_with<W: Write>(
    args: &RecallArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    if !cfg.memory.enabled {
        // Disabled means the bank reports empty, exactly like an empty one:
        // nothing is printed, exit 0.
        return Ok(0);
    }

    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);
    let mut entries: Vec<(Entry, MemoryScope)> =
        list_scoped(MemoryScope::Private, repo, &state, &slug, &cfg)?
            .into_iter()
            .map(|(_, e)| (e, MemoryScope::Private))
            .collect();
    entries.extend(
        list_scoped(MemoryScope::Shared, repo, &state, &slug, &cfg)?
            .into_iter()
            .map(|(_, e)| (e, MemoryScope::Shared)),
    );

    if let Some(key) = &args.key {
        entries.retain(|(entry, _)| &entry.key == key);
    }
    if let Some(days) = args.stale {
        // Saturating, not plain multiplication: `--stale` is an operator-typed
        // `u64`, and anything past `u64::MAX / 86_400` overflowed -- a panic in
        // a debug build, a wrapped (tiny) threshold in a release one, which
        // silently reported every entry as fresh.
        let threshold = now_secs().saturating_sub(days.saturating_mul(86_400));
        entries.retain(|(entry, _)| entry.verified < threshold);
    }

    for (entry, scope) in &entries {
        let label = match scope {
            MemoryScope::Private => "local",
            MemoryScope::Shared => "repo",
        };
        if args.json {
            let scoped = RecallEntry {
                entry,
                scope: label,
            };
            writeln!(w, "{}", serde_json::to_string(&scoped)?)?;
        } else {
            let now = now_secs();
            let written_days = now.saturating_sub(entry.written) / 86_400;
            let verified_days = now.saturating_sub(entry.verified) / 86_400;
            let trust_note = match scope {
                MemoryScope::Shared => " -- repo: repository-owned content, not operator-verified",
                MemoryScope::Private => "",
            };
            writeln!(
                w,
                "{} [{label}{trust_note}] (written {written_days}d ago, verified {verified_days}d ago)\n{}\n",
                entry.key, entry.body
            )?;
        }
    }
    Ok(0)
}
```

- [ ] **Step 4: Run the new tests to verify they pass**

Run: `cargo test --lib commands::ctx::memory::tests::ctx_recall -- --test-threads 8`
Expected: all pass.

- [ ] **Step 5: Run the pre-existing recall tests to confirm no regression**

Run: `cargo test --lib commands::ctx::memory:: --test-threads 8`
Expected: all pass — every pre-existing recall test only ever populated the private bank via `remember`, so the shared half of the new merge simply contributes nothing for them and output is unchanged.

- [ ] **Step 6: Run the full verify suite**

Run, in order: `cargo build`; `cargo nextest run --no-fail-fast --test-threads 8`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`.
Expected: no new failures beyond the known pre-existing `commands::ctx::wrap::tests` set.

- [ ] **Step 7: Commit**

```bash
git add src/commands/ctx/memory.rs
git commit -m "feat(ctx): merge the local and repo memory banks in zirv ctx recall, labeling provenance"
```

---

### Task 4: `zirv ctx forget --repo`

**Files:**
- Modify: `src/commands/ctx/memory.rs:1735-1742` (`ForgetArgs`), `1896-1922` (`run_forget_with`)
- Modify: `src/commands/ctx/memory.rs:2800-2803, 2820-2823` (existing `ForgetArgs { .. }` test literals)
- Test: `src/commands/ctx/memory.rs` (`#[cfg(test)] mod tests`, same file)

**Interfaces:**
- Consumes: `forget_scoped(scope, repo, state, slug, key) -> CtxResult<bool>` (memory.rs:834), unchanged.
- Produces: `ForgetArgs.repo: bool`, consumed by Task 5's docs.

- [ ] **Step 1: Add the `--repo` field to `ForgetArgs`**

```rust
#[derive(Debug, clap::Args)]
pub struct ForgetArgs {
    /// Key to forget. Omit when passing `--all`.
    pub key: Option<String>,
    /// Remove every entry in this repository's memory bank.
    #[arg(long, default_value_t = false)]
    pub all: bool,
    /// Forget from the shared, repository-owned bank (`<repo>/.zirv/memory/`)
    /// instead of the private, machine-local one (issue #172). Not
    /// supported together with `--all`, since `forget_all` only ever clears
    /// the private bank.
    #[arg(long, default_value_t = false)]
    pub repo: bool,
}
```

- [ ] **Step 2: Fix the two existing `ForgetArgs { .. }` test literals**

Add `repo: false,` to the literals at memory.rs:2800 and memory.rs:2820.

- [ ] **Step 3: Write the failing tests**

```rust
    #[test]
    fn ctx_forget_repo_removes_a_shared_entry_without_touching_the_local_one() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_dir = repo.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let cfg = CtxConfig::default();
        let slug = repo_slug(repo.path());

        remember(&state, &slug, &sample("build-cmd", 1), &cfg).expect("remember private");
        upsert_scoped(
            MemoryScope::Shared,
            repo.path(),
            &state,
            &slug,
            &cfg,
            &sample("build-cmd", 1),
        )
        .expect("upsert shared");

        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = ForgetArgs {
            key: Some("build-cmd".to_string()),
            all: false,
            repo: true,
        };
        let mut out = Vec::new();
        run_forget_with(&args, &mut out, repo.path(), &|k| env.get(k).cloned()).expect("forget");

        assert!(
            !repo.path().join(".zirv/memory/build-cmd.md").exists(),
            "the shared entry must be removed"
        );
        assert!(
            get(&state, &slug, "build-cmd").expect("get").is_some(),
            "the local entry must be untouched"
        );
    }

    #[test]
    fn ctx_forget_all_with_repo_is_refused() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_dir = repo.path().join("state");
        let env = env_map(&[(state::STATE_ENV, state_dir.to_str().expect("utf8"))]);
        let args = ForgetArgs {
            key: None,
            all: true,
            repo: true,
        };
        let mut out = Vec::new();
        let err = run_forget_with(&args, &mut out, repo.path(), &|k| env.get(k).cloned())
            .expect_err("--all --repo is not supported");
        assert!(err.to_string().contains("--repo"), "got {err}");
    }
```

- [ ] **Step 4: Run the new tests to verify they fail**

Run: `cargo test --lib commands::ctx::memory::tests::ctx_forget -- --test-threads 8`
Expected: FAIL to compile (`ForgetArgs` has no `repo` field yet).

- [ ] **Step 5: Implement scope-aware forget**

Replace `run_forget_with`'s body (memory.rs:1896-1922):

```rust
pub fn run_forget_with<W: Write>(
    args: &ForgetArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // Deliberately does not check `cfg.memory.enabled`: forgetting must
    // still work while the bank is disabled, the same way disabling a
    // feature must never trap data behind it.
    let state = StateDir::resolve(env)?;
    let slug = repo_slug(repo);

    if args.all {
        if args.repo {
            return Err(
                "zirv ctx forget: --all clears only the local bank; pass a --key with --repo to remove one shared entry instead"
                    .into(),
            );
        }
        forget_all(&state, &slug)?;
        writeln!(w, "zirv ctx forget: cleared the memory bank")?;
        return Ok(0);
    }
    let Some(key) = &args.key else {
        return Err("zirv ctx forget: pass a key, or --all".into());
    };
    let scope = if args.repo {
        MemoryScope::Shared
    } else {
        MemoryScope::Private
    };
    let bank_label = if args.repo { "repo" } else { "local" };
    if forget_scoped(scope, repo, &state, &slug, key)? {
        writeln!(w, "zirv ctx forget: removed '{key}' from the {bank_label} bank")?;
    } else {
        writeln!(w, "zirv ctx forget: no entry for '{key}' in the {bank_label} bank")?;
    }
    Ok(0)
}
```

- [ ] **Step 6: Run the new tests to verify they pass**

Run: `cargo test --lib commands::ctx::memory::tests::ctx_forget -- --test-threads 8`
Expected: all pass.

- [ ] **Step 7: Run the full memory test module**

Run: `cargo test --lib commands::ctx::memory:: --test-threads 8`
Expected: all pass (no test asserted the old exact success/error message text).

- [ ] **Step 8: Run the full verify suite**

Run, in order: `cargo build`; `cargo nextest run --no-fail-fast --test-threads 8`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`.
Expected: no new failures beyond the known pre-existing `commands::ctx::wrap::tests` set.

- [ ] **Step 9: Commit**

```bash
git add src/commands/ctx/memory.rs
git commit -m "feat(ctx): add --repo to zirv ctx forget, targeting the shared memory bank"
```

---

### Task 5: Documentation, version bump, and PR

**Files:**
- Modify: `docs/obsidian/Modules/Ctx Subsystem.md`
- Modify: `docs/obsidian/Concepts/Untrusted Configuration.md`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: the finished behavior from Tasks 1-4 (no code interfaces produced by this task).

- [ ] **Step 1: Update the verb summary in `Ctx Subsystem.md`**

In `docs/obsidian/Modules/Ctx Subsystem.md`, replace the `remember`/`recall`/`forget` bullet (currently around line 42):

```markdown
- **`remember`** / **`recall`** / **`forget`** — read and write the repo-scoped cross-session memory bank (durable facts, not task state — see the Sessions and Memory section below). The newer, scope-aware top-level `zirv memory status`/`list`/`recall`/`remember`/`forget`/`verify` (issue #33, `memory_cli.rs`) is a sibling surface, not a `zirv ctx` verb — see [[Built-in Commands]].
```

with:

```markdown
- **`remember`** / **`recall`** / **`forget`** — read and write the repo-scoped cross-session memory bank (durable facts, not task state — see the Sessions and Memory section below). `remember --repo`/`forget --repo` (issue #172) write to (or remove from) the shared, repository-owned bank instead of the private one, the same `MemoryScope::Shared` `.zirv/memory/` store `zirv memory --shared` already uses; `recall` always merges both banks and labels each entry `[local]`/`[repo — repository-owned content, not operator-verified]`. A shared `remember --repo` is refused outright if the key or body looks credential-shaped (`memory::sensitive_shared_match`, a deny-list plus `review::detect_token_shape`'s reused token-shape regex) unless `--allow-sensitive` is also given. The newer, scope-aware top-level `zirv memory status`/`list`/`recall`/`remember`/`forget`/`verify` (issue #33, `memory_cli.rs`) is a sibling surface, not a `zirv ctx` verb — see [[Built-in Commands]].
```

- [ ] **Step 2: Add a dedicated paragraph to the "Sessions and Memory" section**

In the same file, immediately after the paragraph beginning "**The handoff-vs-memory boundary is deliberate**" (currently ending just before `## See Also`, around line 300), add:

```markdown
  **`--repo`/`--allow-sensitive` on `zirv ctx remember`/`forget`, and merged `zirv ctx recall` (issue #172).** Unlike `zirv memory remember`/`forget --shared` (issue #33's own scope-generic surface), `zirv ctx remember`/`forget` defaulted to the private scope only until this issue; both now take `--repo` to target the shared bank instead (`upsert_scoped`/`forget_scoped` with `MemoryScope::Shared`, unchanged from the store's own contract described above). `zirv ctx recall` no longer reads only the private bank: it merges `list_scoped(Private, ...)` and `list_scoped(Shared, ...)`, private entries first, and labels every rendered or JSON entry with its provenance (`local`/`repo`) — never trusting a shared entry's own header for that label, the same rule `memory_cli.rs`'s `ScopedEntry` already follows. A `--key` lookup checks both banks and can surface both a local and a repo entry for the same key side by side, local one first, rather than picking one. **The new secrets guard** lives in `upsert_shared` itself (`memory::sensitive_shared_match`), so every shared writer gets it: a case-insensitive deny-list term in the key or body (`password`, `passwd`, `secret`, `token`, `api-key`/`api_key`/`apikey`, `credential`, `creds`, `private-key`/`private_key`) or a match against `review::detect_token_shape`'s existing token-shape regex (OpenAI `sk-`, GitHub `ghp_`/`gho_`/`github_pat_`, Slack `xox[baprs]-`, AWS `AKIA`/`ASIA`, a PEM private-key block, or a JWT — widened to `pub(crate)` for this rather than duplicated) refuses the write. `zirv ctx remember --repo --allow-sensitive` (`upsert_shared_allow_sensitive`) is the only bypass, and only there — every automatic writer (`zirv memory remember --shared`, harvesting, `zirv memory optimize --apply`'s consolidation) still goes through the guarded `upsert_scoped`/`upsert_shared` path with no override, and `write_durable`'s own per-candidate harvest loop treats a flagged candidate as a skip-and-log (`harvest-skipped`, the same idiom an already-`explicit` key already gets) rather than aborting the whole batch. Private-scope writes never consult this guard at all — it lives entirely inside `upsert_shared`, not `remember`.
```

- [ ] **Step 3: Bump `Ctx Subsystem.md`'s `last-verified` date**

Update the frontmatter at the top of the file (`last-verified: 2026-08-27`) to the date this task is actually completed.

- [ ] **Step 4: Add a bullet to `Untrusted Configuration.md`'s Memory section**

In `docs/obsidian/Concepts/Untrusted Configuration.md`, in the "Memory: repository facts, capped, and harvested only with consent" section, immediately after the "**Consent-gated, not just labeled.**" bullet (currently ending "...an unreviewed wrong answer landing in a bank every future session reads is worse than simply not harvesting."), add a new bullet:

```markdown
- **Explicit human writes are guarded too, not just automatic ones (issue #172).** `zirv ctx remember --repo` (and `zirv memory remember --shared`, the same sink) refuses to write a shared entry whose key or body looks credential-shaped: a case-insensitive deny-list (`password`, `passwd`, `secret`, `token`, `api-key`/`api_key`/`apikey`, `credential`, `creds`, `private-key`/`private_key`) plus `src/commands/workflow/review.rs`'s existing token-shape regex (OpenAI/GitHub/Slack/AWS-style keys, a PEM block, a JWT), reused rather than duplicated. `--allow-sensitive` on `zirv ctx remember` is the only way past it, and only there — every automatic writer (harvesting, `zirv memory optimize --apply`) still goes through the guarded path with no override, and a flagged harvest candidate is skipped and logged (`harvest-skipped`) rather than aborting the rest of the batch. This is a *write-time* refusal, not a read-time redaction: it does not change what an existing, already-committed shared entry looks like once injected, only what a new one is allowed to become.
```

- [ ] **Step 5: Bump `Untrusted Configuration.md`'s `last-verified` date**

Same as Step 3, for this file's frontmatter.

- [ ] **Step 6: Bump the crate version**

In `Cargo.toml`, change `version = "2.32.0"` to `version = "2.33.0"` (or higher, if a concurrent PR has already claimed `2.33.0` against the same base — check `git log --oneline -1 -- Cargo.toml` on the base branch first).

- [ ] **Step 7: Run the full verify suite one last time**

Run, in order: `cargo build`; `cargo nextest run --no-fail-fast --test-threads 8`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`.
Expected: no new failures beyond the known pre-existing `commands::ctx::wrap::tests` set.

- [ ] **Step 8: Commit the docs and version bump**

```bash
git add docs/obsidian/Modules/"Ctx Subsystem.md" docs/obsidian/Concepts/"Untrusted Configuration.md" Cargo.toml
git commit -m "docs: document repo-portable memories and its secrets guard (issue #172)"
```

- [ ] **Step 9: Push and open the PR**

```bash
git push -u origin feat/172-repo-portable-memories
gh pr create --title "ctx: repo-portable memories with a secrets guard" --body "$(cat <<'EOF'
## Summary
- `zirv ctx remember --repo` writes to `.zirv/memory/<key>.md`, committed with the repo; `zirv ctx forget --repo` removes from it.
- `zirv ctx recall` now merges the local and repo banks, labeling each entry's provenance.
- A new write-time secrets guard refuses a credential-shaped key or body on the shared bank (deny-list plus reused token-shape detection), bypassable only via an explicit `--allow-sensitive` flag on `remember --repo`.

## Test plan
- [ ] `cargo build`
- [ ] `cargo nextest run --no-fail-fast`
- [ ] `cargo test --verbose -- --test-threads=1`
- [ ] `cargo fmt -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`

Closes #172
EOF
)"
```

---

## Self-Review Notes

- **Spec coverage:** `remember --repo` writing to `.zirv/memory/<k>.md` — Task 1. `recall` merging both banks with provenance labels — Task 3. Repo-sourced entries keeping the existing UNTRUSTED/capped/narrow-only posture — unchanged by this plan (already true of `MemoryScope::Shared`; Task 5 documents it, no code path in this plan grants a shared entry any new authority). Secrets guard — Task 2. Branch semantics accepted as-is — no task adds git-awareness, matching the spec's own acceptance of the status quo.
- **Placeholder scan:** every step above shows real, file-and-line-anchored code; no "TBD"/"handle appropriately" text.
- **Type/signature consistency:** `upsert_shared`'s new `allow_sensitive: bool` parameter (Task 2) is threaded through its one call site inside `upsert_scoped` and its one new public wrapper `upsert_shared_allow_sensitive`; `RememberArgs.repo`/`allow_sensitive` and `ForgetArgs.repo` are each added exactly once and every existing struct literal across `memory.rs`/`memory_cli.rs` that constructs them is updated in the same task.
- **Known collateral audited, not guessed:** a dedicated research pass confirmed the guard's only pre-existing test/production collisions are `sample()`'s fixed body (Task 2, Step 2), one `sample("staging-db-creds", ...)` key-only collision (Task 2, Step 3), one hand-inlined harvest test (Task 2, Step 4), and one shell fixture line (Task 2, Step 5) — `memory_cli.rs`, `memory_optimize.rs`, `retrieval.rs`, and `compile.rs` need no changes. `memory_optimize.rs`'s `apply_consolidation` already treats a failed `upsert_scoped` as "skip this group" (its own pre-existing `is_ok()` check), so a guard rejection there needs no new code.
