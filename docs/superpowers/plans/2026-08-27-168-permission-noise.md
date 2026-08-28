
# Permission-Noise Reduction (Issue #168) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut the ~119/day approval-prompt volume reported in issue #168 by widening the safety classifier's `--dangerously-disable-sandbox` retry path to recognize read-only `gh`/`glab`/`git`/`curl`/`wget`/`kubectl` commands and `zirv ctx` itself, replacing the invalid-attestation blanket ask/deny with self-healing re-evaluation, letting a harmless leading `cd <known-root>` stop dragging a compound to the mode default, allowing compounds whose writes are confined to the session scratchpad, widening the sandboxed write allowlist to zirv's own state directories, and locking in that read-only `find`/`locate`/`rg` never hard-denies — all backed by a regression corpus.

**Architecture:** All classification logic lives in the existing `src/commands/ctx/safety.rs` module (the pure `evaluate`/`evaluate_candidates` core, plus the hook-mode outer layer `run_check_hook_mode_with_env` that already does its own env/fs I/O for attestation and audit logging). New classifiers are added as either (a) pure per-candidate analyzers folded into the existing `evaluate_candidate_outcome` chain (extracted from `evaluate_candidates` in Task 1), or (b) hook-layer carve-outs alongside the existing `is_sandbox_bypass_safe_gh_command`/`escape_allow_matches` checks in `run_check_hook_mode_with_env`, matching the precedent issue #147 already established. The sandbox write-allowlist change is a small, isolated edit to `src/commands/ctx/adapters/claude.rs`'s `launch_settings_value`/`launch_settings_path`, reusing `StateDir` accessors from `src/commands/ctx/state.rs`.

**Tech Stack:** Rust edition 2024, serde/serde_json, sha2, `#[cfg(test)]` inline unit tests, no new dependencies.

**Spec:** https://github.com/Glubiz/zirv-dynamic-cli/issues/168

## Global Constraints

- Work in the worktree `D:\GitHub\zirv-168` on branch `feat/168-permission-noise`, based on `release/2.32.0`. Do not touch `D:\GitHub\zirv-dynamic-cli`.
- Rust edition 2024. No new crate dependencies.
- Run all five verify commands before claiming any task done, throttled to 8 jobs/threads on this host:
  `cargo build`
  `cargo nextest run --no-fail-fast -j 8`
  `cargo test --verbose -- --test-threads=8`
  `cargo fmt -- --check`
  `cargo clippy --all-targets -- -D warnings`
- Tests stay inline in `#[cfg(test)] mod tests` in the files they cover — no new test files.
- Commit after each task goes green. Never commit/push to `main`/`master`; this plan already assumes work happens on `feat/168-permission-noise`.
- Bump `Cargo.toml`'s version above its base (`release/2.32.0`'s own version) before the branch's PR — do this once, in the final docs task, so every intermediate commit stays buildable without a half-bumped version.
- No `Co-Authored-By` or "Generated with Claude Code" lines in commits.
- `src/commands/ctx/adapters/claude.rs`'s `#[cfg(not(windows))]` sandbox block (touched in Task 7) must additionally be verified on Linux/Docker per this repo's own convention (see Task 7's final step) before the branch is considered done — this machine is Windows and cannot run those `#[cfg(not(windows))]` tests at all.
- Every one of the seven design decisions below traces to one task: (a) read-only escape-safe classifier -> Task 2; (b) `zirv ctx` always-allow -> Task 3; (c) attestation self-heal -> Task 4; (d) scratchpad-confined writes -> Task 6; (e) `cd <known-root> && <cmd>` -> Task 5; (f) find/locate/rg regression lock -> Task 8; (g) sandbox write allowlist -> Task 7; (h) regression corpus -> Task 9.

---

## Task 1: Extract the shared per-candidate analyzer chain

This is pure refactoring groundwork: Tasks 5 and 6 both need to evaluate one already-normalized candidate string through the exact same analyzer chain `evaluate_candidates`'s own fold loop uses, without a second, drifting copy of the seven `apply_*_outcome` calls.

**Files:**
- Modify: `src/commands/ctx/safety.rs:576-597` (`evaluate_candidates`'s fold loop)
- Test: `src/commands/ctx/safety.rs` (`#[cfg(test)] mod tests`, appended near the other `evaluate_candidates`-adjacent tests around line 4600)

**Interfaces:**
- Produces: `fn evaluate_candidate_outcome(policy: &SafetyPolicy, candidate: &str, fallback: Verdict) -> Outcome` — the exact per-candidate chain `evaluate_candidates` already runs (`evaluate_single` + all seven `apply_*_outcome` calls). Tasks 5 and 6 call this directly.

- [ ] **Step 1: Write the failing test locking today's behavior through the new entry point**

Add this test to `mod tests` in `src/commands/ctx/safety.rs`:

```rust
    /// Task 1 (issue #168): `evaluate_candidate_outcome` must reproduce
    /// exactly what `evaluate_candidates`'s own fold already does for a
    /// single, unambiguous candidate -- this is a refactor extracting the
    /// analyzer chain, not a behavior change.
    #[test]
    fn evaluate_candidate_outcome_matches_the_existing_fold_for_a_single_candidate() {
        let policy = SafetyPolicy::default();
        for (command, expected) in [
            ("git status", Verdict::Allow),
            ("git push --force origin main", Verdict::Ask),
            ("rm -rf /", Verdict::Deny),
            ("some-totally-unknown-tool --flag", Verdict::Ask),
        ] {
            let direct = evaluate_candidate_outcome(&policy, command, Verdict::Ask);
            let via_evaluate_candidates =
                evaluate_candidates(&policy, command, Verdict::Ask, LaunchMode::Headless);
            assert_eq!(direct.verdict, expected, "{command}");
            assert_eq!(
                direct.verdict, via_evaluate_candidates.verdict,
                "{command}: extracted chain must agree with the fold"
            );
        }
    }
```

- [ ] **Step 2: Run it to confirm it fails to compile (the function does not exist yet)**

Run: `cargo test --lib commands::ctx::safety::tests::evaluate_candidate_outcome_matches_the_existing_fold_for_a_single_candidate -- --test-threads=8`
Expected: compile error, `cannot find function 'evaluate_candidate_outcome' in this scope`

- [ ] **Step 3: Extract the analyzer chain**

Above `fn evaluate_candidates` in `src/commands/ctx/safety.rs`, add:

```rust
/// The per-candidate analyzer chain [`evaluate_candidates`]'s own fold loop
/// applies to every normalized executable candidate -- extracted (issue
/// #168) so a caller that needs one candidate's own verdict in isolation
/// (`every_segment_is_allow_or_unmatched_default`, Task 6) can run the
/// identical chain without a second, drifting copy of these seven analyzer
/// calls.
fn evaluate_candidate_outcome(policy: &SafetyPolicy, candidate: &str, fallback: Verdict) -> Outcome {
    let base = evaluate_single(policy, candidate, fallback);
    let outcome = apply_sql_outcome(policy, candidate, base);
    let outcome = apply_credential_outcome(candidate, outcome);
    let outcome = apply_network_outcome(candidate, outcome);
    let outcome = apply_recursive_delete_outcome(candidate, outcome);
    let outcome = apply_vcs_outcome(candidate, outcome);
    let outcome = apply_distribution_outcome(candidate, outcome);
    let outcome = apply_orchestrator_outcome(candidate, outcome);
    let outcome = apply_pipe_to_shell_outcome(candidate, outcome);
    apply_find_exec_outcome(candidate, outcome)
}
```

Then replace the body of `evaluate_candidates`'s `for candidate in candidates` loop (currently lines ~578-597):

```rust
    let mut worst: Option<(u8, Outcome)> = None;
    for candidate in candidates {
        let outcome = evaluate_candidate_outcome(policy, &candidate, fallback);
        let rank = verdict_rank(outcome.verdict);
        let is_worse = match &worst {
            Some((best_rank, _)) => rank > *best_rank,
            None => true,
        };
        if is_worse {
            worst = Some((rank, outcome));
        }
    }
    worst.map(|(_, outcome)| outcome).unwrap_or(Outcome {
        verdict: fallback,
        matched: None,
    })
```

- [ ] **Step 4: Run the new test and the full safety suite**

Run: `cargo test --lib commands::ctx::safety:: -- --test-threads=8`
Expected: PASS, no regressions in the existing ~200 tests in this module.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/safety.rs
git commit -m "refactor(ctx-safety): extract evaluate_candidate_outcome from evaluate_candidates' fold"
```

---

## Task 2: Read-only escape-safe classifier for the sandbox retry path (issue #168, decision a)

**Files:**
- Modify: `src/commands/ctx/safety.rs` (new consts/functions near `SANDBOX_ESCAPE_BUILTIN_PROGRAMS`/`is_sandbox_bypass_safe_gh_command` around line 3652; wiring into `run_check_hook_mode_with_env` around line 4129)
- Test: `src/commands/ctx/safety.rs` (`mod tests`)

**Interfaces:**
- Consumes: `normalize_segments`, `escape_denied_by_screen`, `sql_tokens`, `sql_program_name`, `collapse_whitespace`, `first_positional`, `KUBE_HELM_VALUE_FLAGS`, `SANDBOX_ESCAPE_BUILTIN_PROGRAMS` (all pre-existing in `safety.rs`).
- Produces: `fn is_read_only_escape_safe(command: &str, scratchpad_roots: &[String]) -> bool`, used only inside `run_check_hook_mode_with_env`'s retry branch. Task 6 also produces a `fn target_is_confined(target: &str, scratchpad_roots: &[String]) -> bool` helper this task reuses for `curl -o`/`wget -O` targets — **write that helper in this task** (Task 6 reuses it, does not redefine it).

- [ ] **Step 1: Write the failing classifier unit tests**

Add to `mod tests` in `src/commands/ctx/safety.rs`:

```rust
    // -- is_read_only_escape_safe (issue #168, decision a) ---------------

    #[test]
    fn read_only_escape_safe_qualifies_gh_glab_git_curl_wget_kubectl_read_forms() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in [
            "gh issue view 118",
            "gh pr checks 42",
            "gh api repos/x/y",
            "gh api repos/x/y -X GET",
            "gh api repos/x/y --method GET",
            "glab issue view 1",
            "glab mr diff 1",
            "git status",
            "git log --oneline",
            "git diff --stat",
            "git show HEAD",
            "git branch",
            "git branch -a",
            "git fetch",
            "git fetch origin",
            "git ls-remote",
            "git remote",
            "git remote -v",
            "git remote show origin",
            "git remote get-url origin",
            "git rev-parse HEAD",
            "git describe",
            "git blame src/main.rs",
            "git shortlog",
            "git stash list",
            "git worktree list",
            "git tag",
            "git tag --list",
            "git tag -l",
            "curl https://example.com",
            "curl -o /dev/null https://example.com",
            "curl -o /tmp/claude/out.json https://example.com",
            "wget https://example.com",
            "kubectl get pods",
            "kubectl -n prod get pods",
            "kubectl describe pod x",
            "kubectl logs x",
            "kubectl version",
            "kubectl api-resources",
            "kubectl config view",
        ] {
            assert!(
                is_read_only_escape_safe(command, &roots),
                "{command} should qualify"
            );
        }
    }

    #[test]
    fn read_only_escape_safe_rejects_mutating_or_ambiguous_forms() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in [
            "gh pr create --title x",
            "gh api repos/x/y -X POST",
            "gh api repos/x/y --method DELETE",
            "gh api repos/x/y -f name=value",
            "glab mr create",
            "git branch -d old",
            "git branch -D old",
            "git branch -m new",
            "git push origin main",
            "git reset --hard",
            "curl -X POST https://example.com",
            "curl -d 'a=b' https://example.com",
            "curl -F 'a=b' https://example.com",
            "curl -T file https://example.com",
            "curl -o /etc/passwd https://example.com",
            "kubectl exec -it pod -- sh",
            "kubectl delete pod x",
            "kubectl apply -f x.yaml",
            "kubectl config set-context x",
            "cd /tmp && rm -rf /",
        ] {
            assert!(
                !is_read_only_escape_safe(command, &roots),
                "{command} should not qualify"
            );
        }
    }

    #[test]
    fn read_only_escape_safe_still_screens_credential_paths_and_root_wide_find() {
        let roots = vec!["/tmp/claude".to_string()];
        assert!(!is_read_only_escape_safe("cat ~/.ssh/id_rsa", &roots));
        assert!(!is_read_only_escape_safe("find / -name id_rsa", &roots));
        assert!(is_read_only_escape_safe("grep TODO ./src", &roots));
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib commands::ctx::safety::tests::read_only_escape_safe -- --test-threads=8`
Expected: compile error, `is_read_only_escape_safe` not found.

- [ ] **Step 3: Implement the classifier**

Add below `builtin_escape_allow` (after line ~3670) in `src/commands/ctx/safety.rs`:

```rust
/// Issue #168, design decision (a): the `(noun, verb)` gh/glab forms this
/// broader, tool-agnostic classifier treats as read-only -- a superset of
/// [`SANDBOX_BYPASS_SAFE_GH_FORMS`]'s own gh table, kept separate because
/// that table backs the narrower gh-credential-config carve-out
/// specifically (see its own doc comment), while this one backs
/// [`is_read_only_escape_safe`].
const READ_ONLY_ESCAPE_SAFE_GH_FORMS: &[(&str, &str)] = &[
    ("issue", "view"),
    ("issue", "list"),
    ("pr", "view"),
    ("pr", "list"),
    ("pr", "diff"),
    ("pr", "checks"),
    ("repo", "view"),
    ("run", "view"),
    ("run", "list"),
];

/// The GitLab CLI's equivalent read-only `(noun, verb)` forms.
const READ_ONLY_ESCAPE_SAFE_GLAB_FORMS: &[(&str, &str)] = &[
    ("issue", "view"),
    ("issue", "list"),
    ("mr", "view"),
    ("mr", "list"),
    ("mr", "diff"),
    ("repo", "view"),
    ("ci", "view"),
    ("ci", "status"),
];

/// Whether `tokens` is a read-only `gh`/`glab` invocation: `gh api` with no
/// method other than `GET` and no body flag (`-f`/`-F`/`--input`), or a
/// `(noun, verb)` pair from [`READ_ONLY_ESCAPE_SAFE_GH_FORMS`]/[`READ_ONLY_
/// ESCAPE_SAFE_GLAB_FORMS`]. `--web`/`-w` always disqualifies (an external
/// browser process), mirroring [`is_sandbox_bypass_safe_gh_command`].
fn is_gh_or_glab_read_only(tokens: &[String]) -> bool {
    let Some(program) = tokens.first().map(|t| sql_program_name(t)) else {
        return false;
    };
    if !matches!(program.as_str(), "gh" | "glab") {
        return false;
    }
    if tokens.iter().any(|t| {
        t.starts_with("--web") || (t.starts_with('-') && !t.starts_with("--") && t.contains('w'))
    }) {
        return false;
    }
    if program == "gh" && tokens.get(1).map(String::as_str) == Some("api") {
        let mut method_is_get = true;
        let mut has_body_flag = false;
        let mut i = 2;
        while i < tokens.len() {
            let token = tokens[i].as_str();
            match token {
                "-X" | "--method" => {
                    match tokens.get(i + 1) {
                        Some(value) if value.eq_ignore_ascii_case("GET") => {}
                        _ => method_is_get = false,
                    }
                    i += 1;
                }
                "-f" | "-F" | "--input" => has_body_flag = true,
                _ if token.starts_with("--method=") => {
                    if !token["--method=".len()..].eq_ignore_ascii_case("GET") {
                        method_is_get = false;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        return method_is_get && !has_body_flag;
    }
    let (Some(noun), Some(verb)) = (tokens.get(1), tokens.get(2)) else {
        return false;
    };
    let table = if program == "gh" {
        READ_ONLY_ESCAPE_SAFE_GH_FORMS
    } else {
        READ_ONLY_ESCAPE_SAFE_GLAB_FORMS
    };
    table
        .iter()
        .any(|&(n, v)| n == noun.as_str() && v == verb.as_str())
}

/// Issue #168, design decision (a): git subcommands that can only read --
/// `branch`/`remote`/`tag` are further restricted to their non-mutating
/// forms, since the bare subcommand name also accepts destructive flags
/// (`branch -d`, `remote add`, ...).
fn is_git_read_only(tokens: &[String]) -> bool {
    if tokens.first().map(|t| sql_program_name(t)).as_deref() != Some("git") {
        return false;
    }
    let Some(sub) = tokens.get(1).map(String::as_str) else {
        return false;
    };
    match sub {
        "status" | "log" | "diff" | "show" | "fetch" | "ls-remote" | "rev-parse" | "describe"
        | "blame" | "shortlog" => true,
        "branch" => !tokens.iter().skip(2).any(|t| {
            matches!(t.as_str(), "-d" | "-D" | "-m" | "-M")
                || t == "--set-upstream"
                || t.starts_with("--set-upstream=")
        }),
        "remote" => {
            let rest: Vec<&str> = tokens.iter().skip(2).map(String::as_str).collect();
            rest.is_empty()
                || rest == ["-v"]
                || rest.first() == Some(&"show")
                || rest.first() == Some(&"get-url")
        }
        "stash" => tokens.get(2).map(String::as_str) == Some("list"),
        "worktree" => tokens.get(2).map(String::as_str) == Some("list"),
        "tag" => {
            let rest: Vec<&str> = tokens.iter().skip(2).map(String::as_str).collect();
            rest.is_empty() || rest.first() == Some(&"--list") || rest.first() == Some(&"-l")
        }
        _ => false,
    }
}

/// Issue #168, design decision (a): whether `target` is `/dev/null` or
/// lexically beneath one of `scratchpad_roots` (already forward-slash
/// normalized, no trailing separator). A target carrying `$`, a backtick,
/// `~`, or a shell glob character is never treated as confined -- this
/// classifier is text-only and cannot know what such a target expands to.
/// Reused by [`is_curl_or_wget_get_only`] (this task) and by [`write_
/// targets_confined`] (Task 6).
fn target_is_confined(target: &str, scratchpad_roots: &[String]) -> bool {
    if target == "/dev/null" {
        return true;
    }
    if target.contains(['$', '`', '~', '*', '?']) {
        return false;
    }
    let normalized = target.replace('\\', "/");
    scratchpad_roots
        .iter()
        .any(|root| !root.is_empty() && normalized.starts_with(root.as_str()))
}

/// Issue #168, design decision (a): a GET-only `curl`/`wget` -- no `-X`/
/// `--request` other than `GET`, no body-uploading flag, and any `-o`/
/// `-O`/`--output` target confined per [`target_is_confined`].
fn is_curl_or_wget_get_only(tokens: &[String], scratchpad_roots: &[String]) -> bool {
    let Some(program) = tokens.first().map(|t| sql_program_name(t)) else {
        return false;
    };
    if !matches!(program.as_str(), "curl" | "wget") {
        return false;
    }
    let mut i = 1;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        match token {
            "-X" | "--request" => {
                match tokens.get(i + 1) {
                    Some(value) if value.eq_ignore_ascii_case("GET") => {}
                    _ => return false,
                }
                i += 1;
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-urlencode" | "-F"
            | "--form" | "-T" | "--upload-file" => return false,
            // `-O`/`--remote-name` derives its output filename from the URL
            // and writes it into the current directory -- there is no
            // explicit target argument for this classifier to confine at
            // all, so it can never be proven scratchpad-confined and always
            // disqualifies, matching the design decision's "-o/-O/--output
            // allowed only ... under the scratchpad" (an unprovable target
            // is not a confined one).
            "-O" | "--remote-name" => return false,
            "-o" | "--output" => {
                let Some(target) = tokens.get(i + 1) else {
                    return false;
                };
                if !target_is_confined(target, scratchpad_roots) {
                    return false;
                }
                i += 1;
            }
            _ if token.starts_with("--data") => return false,
            _ => {}
        }
        i += 1;
    }
    true
}

/// Issue #168, design decision (a): the read-only `kubectl` verbs -- `get`/
/// `describe`/`logs`/`version`/`api-resources` outright, `config view`
/// (never a bare `config`, which also accepts `set-context`/`use-context`
/// mutations). Reuses [`first_positional`]/[`KUBE_HELM_VALUE_FLAGS`] so a
/// global flag ahead of the verb (`kubectl -n prod get pods`) is not
/// misread as the verb itself.
fn is_kubectl_read_only(tokens: &[String]) -> bool {
    if tokens.first().map(|t| sql_program_name(t)).as_deref() != Some("kubectl") {
        return false;
    }
    let Some(verb) = first_positional(tokens, KUBE_HELM_VALUE_FLAGS) else {
        return false;
    };
    match verb {
        "get" | "describe" | "logs" | "version" | "api-resources" => true,
        "config" => {
            let config_index = tokens.iter().position(|t| t == verb).unwrap_or(0);
            first_positional(&tokens[config_index..].to_vec(), KUBE_HELM_VALUE_FLAGS) == Some("view")
        }
        _ => false,
    }
}

/// Issue #168, design decision (a): whether EVERY executable segment of the
/// retried `command` is a read-only `gh`/`glab` call, a read-only git
/// subcommand, a GET-only `curl`/`wget`, a read-only `kubectl` verb, or one
/// of the existing [`SANDBOX_ESCAPE_BUILTIN_PROGRAMS`] -- used ONLY on the
/// `--dangerously-disable-sandbox` retry path (`run_check_hook_mode_with_
/// env`), alongside `is_sandbox_bypass_safe_gh_command`/`escape_allow_
/// matches`/`is_zirv_ctx_escape_safe`. Reuses [`normalize_segments`]'s own
/// decomposition and [`escape_denied_by_screen`]'s credential/root-scan
/// gate, exactly like [`escape_allow_matches`] -- a single disqualifying
/// segment fails the whole command. Never applied when the base verdict is
/// already `Deny` (see the call site).
pub(crate) fn is_read_only_escape_safe(command: &str, scratchpad_roots: &[String]) -> bool {
    let candidates = normalize_segments(command);
    if candidates.is_empty() {
        return false;
    }
    candidates.iter().all(|candidate| {
        if escape_denied_by_screen(candidate) {
            return false;
        }
        let Some(tokens) = sql_tokens(&collapse_whitespace(candidate)) else {
            return false;
        };
        if tokens.is_empty() {
            return false;
        }
        let program = sql_program_name(&tokens[0]);
        if SANDBOX_ESCAPE_BUILTIN_PROGRAMS.contains(&program.as_str()) {
            return true;
        }
        is_gh_or_glab_read_only(&tokens)
            || is_git_read_only(&tokens)
            || is_curl_or_wget_get_only(&tokens, scratchpad_roots)
            || is_kubectl_read_only(&tokens)
    })
}
```

- [ ] **Step 4: Run the classifier tests**

Run: `cargo test --lib commands::ctx::safety::tests::read_only_escape_safe -- --test-threads=8`
Expected: PASS

- [ ] **Step 5: Wire the classifier into the retry branch**

In `src/commands/ctx/safety.rs`'s `run_check_hook_mode_with_env` (around line 4129), the retry branch currently reads:

```rust
    if payload.tool_input.dangerously_disable_sandbox && outcome.verdict != Verdict::Deny {
        outcome = if outcome.verdict == Verdict::Allow && is_sandbox_bypass_safe_gh_command(command)
        {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: read-only gh>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if outcome.verdict == Verdict::Allow
            && escape_allow_matches(&cfg.safety.escape_allow, command)
        {
```

Add a new arm between the two existing ones, and compute `scratchpad_roots` once above the `if` (this same binding is reused by Tasks 3, 5, 6 -- add it now, in this task, since it is this task's own new dependency):

```rust
    let scratchpad_roots = vec![scratchpad_write_root(&std::env::temp_dir())];
    if payload.tool_input.dangerously_disable_sandbox && outcome.verdict != Verdict::Deny {
        outcome = if outcome.verdict == Verdict::Allow && is_sandbox_bypass_safe_gh_command(command)
        {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: read-only gh>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if outcome.verdict == Verdict::Allow
            && is_read_only_escape_safe(command, &scratchpad_roots)
        {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: read-only escape>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if outcome.verdict == Verdict::Allow
            && escape_allow_matches(&cfg.safety.escape_allow, command)
        {
```

(the closing `} else { degrade }` arm is unchanged). Add the small helper this introduces, just above `run_check_hook_mode_with_env`:

```rust
/// Issue #168: the actual filesystem scratchpad root (`<temp_dir>/claude`)
/// a write/output target must fall beneath to count as session-scratchpad-
/// confined -- forward-slash-normalized, no trailing separator, the same
/// literal path segment `adapters::scratchpad_rules` projects into its own
/// `//<path>/claude/**` claude permission-rule form (see that function's own
/// doc comment). Computed here, in the hook-mode outer layer that already
/// does its own clock/fs/env I/O -- never inside `evaluate`/`evaluate_
/// candidates`, which stay pure.
fn scratchpad_write_root(temp_dir: &std::path::Path) -> String {
    let normalized = temp_dir.to_string_lossy().replace('\\', "/");
    format!("{}/claude", normalized.trim_end_matches('/'))
}
```

- [ ] **Step 6: Write end-to-end hook-mode tests for the retry carve-out**

Add to `mod tests`:

```rust
    /// End-to-end: a read-only kubectl/curl/git/glab retry allows silently
    /// in both modes; the non-read-only sibling still escalates.
    #[test]
    fn an_unsandboxed_retry_of_a_read_only_escape_safe_command_allows_silently() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        for command in [
            "kubectl get pods",
            "curl https://example.com",
            "git fetch origin",
            "glab mr diff 1",
        ] {
            let stdin = format!(
                r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":true}},"permission_mode":"default"}}"#
            );
            let mut out = Vec::new();
            run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert!(
                text.contains(r#""permissionDecision":"allow""#),
                "{command}: got {text}"
            );
            assert!(!text.contains("unsandboxed retry"), "{command}: got {text}");
        }
    }

    #[test]
    fn an_unsandboxed_retry_of_a_mutating_kubectl_or_curl_command_still_escalates() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        for command in ["kubectl delete pod x", "curl -X POST https://example.com"] {
            for (mode, expected) in [("default", "ask"), ("dontAsk", "deny")] {
                let stdin = format!(
                    r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":true}},"permission_mode":"{mode}"}}"#
                );
                let mut out = Vec::new();
                run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
                let text = String::from_utf8(out).expect("utf8");
                assert!(
                    text.contains(&format!(r#""permissionDecision":"{expected}""#)),
                    "{command} mode {mode}: got {text}"
                );
            }
        }
    }
```

- [ ] **Step 7: Run the full safety test module and the other four verify commands**

Run: `cargo test --lib commands::ctx::safety:: -- --test-threads=8`
Expected: PASS
Run: `cargo build`, `cargo nextest run --no-fail-fast -j 8`, `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`
Expected: all pass (compare any failing test NAMEs against this machine's known pre-existing `wrap::` baseline per `CLAUDE.md` before treating anything as a regression)

- [ ] **Step 8: Commit**

```bash
git add src/commands/ctx/safety.rs
git commit -m "feat(ctx-safety): allow read-only gh/glab/git/curl/wget/kubectl on sandbox retry"
```

---

## Task 3: `zirv ctx` (and bare `zirv help`/`version`/`memory`/`context`) always allow on retry (decision b)

**Files:**
- Modify: `src/commands/ctx/safety.rs` (new classifier near Task 2's; wiring in `run_check_hook_mode_with_env`)
- Test: `src/commands/ctx/safety.rs` (`mod tests`)

**Interfaces:**
- Consumes: `normalize_segments`, `sql_tokens`, `sql_program_name`, `collapse_whitespace` (pre-existing).
- Produces: `fn is_zirv_ctx_escape_safe(command: &str) -> bool`.

- [ ] **Step 1: Write the failing tests**

```rust
    // -- is_zirv_ctx_escape_safe (issue #168, decision b) -----------------

    #[test]
    fn zirv_ctx_escape_safe_qualifies_ctx_and_bare_builtins() {
        for command in [
            "zirv ctx status",
            "zirv ctx remember foo bar",
            "zirv ctx send --to worker-1 hello",
            "zirv help",
            "zirv version",
            "zirv memory",
            "zirv context",
            "zirv ctx status && zirv ctx remember foo",
        ] {
            assert!(is_zirv_ctx_escape_safe(command), "{command} should qualify");
        }
    }

    #[test]
    fn zirv_ctx_escape_safe_rejects_agent_chat_and_arbitrary_scripts() {
        for command in [
            "zirv agent codex \"do the thing\"",
            "zirv chat",
            "zirv build",
            "zirv ctx status && rm -rf /",
            "zirv",
        ] {
            assert!(
                !is_zirv_ctx_escape_safe(command),
                "{command} should not qualify"
            );
        }
    }

    #[test]
    fn an_unsandboxed_retry_of_zirv_ctx_allows_silently_even_headless() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        for (command, mode) in [
            ("zirv ctx status", "default"),
            ("zirv ctx status", "dontAsk"),
        ] {
            let stdin = format!(
                r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":true}},"permission_mode":"{mode}"}}"#
            );
            let mut out = Vec::new();
            run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert!(
                text.contains(r#""permissionDecision":"allow""#),
                "{command} ({mode}): got {text}"
            );
        }
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib commands::ctx::safety::tests::zirv_ctx_escape_safe -- --test-threads=8`
Expected: compile error, function not found.

- [ ] **Step 3: Implement the classifier**

Add next to `is_read_only_escape_safe` in `src/commands/ctx/safety.rs`:

```rust
/// Issue #168, design decision (b): whether EVERY executable segment of
/// `command` is `zirv ctx <anything>`, or one of the bare `zirv help`/
/// `zirv version`/`zirv memory`/`zirv context` built-ins (no further
/// subcommand). `zirv ctx` is what performs the very attestation/safety
/// checks this module implements -- a broken launch snapshot must not lock
/// zirv's own supervision commands out from correcting it (see Task 4's
/// self-heal, which this carve-out complements on the retry path
/// specifically). Deliberately excludes `zirv agent`, `zirv chat`, and any
/// other zirv subcommand or script name that launches a subprocess of its
/// own -- narrower than the blanket `Bash(zirv *)` shipped allow rule.
pub(crate) fn is_zirv_ctx_escape_safe(command: &str) -> bool {
    let candidates = normalize_segments(command);
    if candidates.is_empty() {
        return false;
    }
    candidates.iter().all(|candidate| {
        let Some(tokens) = sql_tokens(&collapse_whitespace(candidate)) else {
            return false;
        };
        let Some(first) = tokens.first() else {
            return false;
        };
        if sql_program_name(first) != "zirv" {
            return false;
        }
        match tokens.get(1).map(String::as_str) {
            Some("ctx") => true,
            Some("help" | "version" | "memory" | "context") => tokens.len() == 2,
            _ => false,
        }
    })
}
```

- [ ] **Step 4: Run the classifier tests**

Run: `cargo test --lib commands::ctx::safety::tests::zirv_ctx_escape_safe -- --test-threads=8`
Expected: classifier tests PASS; the end-to-end hook test still fails (not wired yet).

- [ ] **Step 5: Wire into the retry branch**

In `run_check_hook_mode_with_env`, add another arm (order among the `Allow`-gated arms does not matter -- they are mutually exclusive by construction, but keep it readable by placing it right before Task 2's `is_read_only_escape_safe` arm, as shown):

```rust
        } else if outcome.verdict == Verdict::Allow && is_zirv_ctx_escape_safe(command) {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: zirv ctx>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if outcome.verdict == Verdict::Allow
            && is_read_only_escape_safe(command, &scratchpad_roots)
        {
```

- [ ] **Step 6: Run the full safety module tests**

Run: `cargo test --lib commands::ctx::safety:: -- --test-threads=8`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/commands/ctx/safety.rs
git commit -m "feat(ctx-safety): always allow zirv ctx on the sandbox retry path"
```

---

## Task 4: Attestation self-heal instead of blanket ask/deny (decision c)

**Files:**
- Modify: `src/commands/ctx/safety.rs:335-410` (`evaluate_with_attestation_evidence`)
- Test: `src/commands/ctx/safety.rs` (`mod tests`, replacing/extending the existing attestation tests around line 4329)

**Interfaces:**
- Produces: `fn self_healed_evaluation(current: &SafetyPolicy, command: &str, mode: LaunchMode, current_fingerprint: String, launch_fingerprint: Option<String>, snapshot_path: Option<&str>) -> AttestedEvaluation` and `fn rematerialize_policy_snapshot(path: &str, policy: &SafetyPolicy) -> std::io::Result<()>`.
- A new `AttestedEvaluation.status` value, `"self-healed"`, alongside the existing `"not-present"`/`"invalid"`/`"valid"`. Only `audit_hook_decision`'s log record reads this field today, so no other call site needs updating.

- [ ] **Step 1: Rewrite the existing tampering test to expect self-heal, and add new ones**

Replace the existing `an_attested_launch_keeps_the_stricter_policy_and_fails_closed_on_tampering` test (lines ~4329-4369) with:

```rust
    /// Issue #168, design decision (c): a widened-and-tampered snapshot no
    /// longer fails the whole session closed -- it self-heals to the
    /// current, in-process policy (the trusted source: this same process
    /// already resolved it from `~/.zirv/ctx.toml` and the repo's own
    /// `.zirv/ctx.toml`), and never LOOSENS beyond what the current policy
    /// itself would allow.
    #[test]
    fn a_tampered_attestation_snapshot_self_heals_to_the_current_policy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = tmp.path().join("policy.json");
        let launch = SafetyPolicy::default();
        std::fs::write(
            &snapshot,
            serde_json::to_string(&launch).expect("serializes"),
        )
        .expect("writes");
        let fingerprint = policy_fingerprint(&launch).expect("fingerprints");
        let env = env_from(&[
            (POLICY_FINGERPRINT_ENV, &fingerprint),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);

        std::fs::write(&snapshot, "{}").expect("tamper snapshot");
        let current = SafetyPolicy::default();
        for (mode, command, expected) in [
            (LaunchMode::Interactive, "cargo test", Verdict::Allow),
            (LaunchMode::Headless, "cargo test", Verdict::Ask),
            (LaunchMode::Headless, "rm -rf /", Verdict::Deny),
        ] {
            let evidence =
                evaluate_with_attestation_evidence(&current, command, mode, &|k| env.get(k).cloned());
            assert_eq!(
                evidence.outcome.verdict, expected,
                "{mode:?} {command}: {evidence:?}"
            );
            assert_eq!(evidence.status, "self-healed");
        }
    }

    /// The self-heal must never widen past what `current` itself already
    /// says: a policy an operator has explicitly NARROWED still denies,
    /// even with a broken snapshot on disk.
    #[test]
    fn a_tampered_attestation_snapshot_still_honors_a_narrowed_current_policy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = tmp.path().join("policy.json");
        std::fs::write(&snapshot, "not valid json at all").expect("writes garbage");
        let env = env_from(&[
            (POLICY_FINGERPRINT_ENV, "irrelevant-since-file-is-unreadable"),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);

        let mut narrowed = SafetyPolicy::default();
        narrowed.deny.push(Rule {
            pattern: "terraform destroy*".to_string(),
            origin: Origin::Operator,
        });
        let evidence = evaluate_with_attestation_evidence(
            &narrowed,
            "terraform destroy",
            LaunchMode::Interactive,
            &|k| env.get(k).cloned(),
        );
        assert_eq!(evidence.outcome.verdict, Verdict::Deny);
        assert_eq!(evidence.status, "self-healed");
    }

    /// Self-heal best-effort re-materializes the snapshot file so the NEXT
    /// command in the same session attests cleanly again.
    #[test]
    fn self_heal_rewrites_the_snapshot_file_from_the_current_policy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = tmp.path().join("nested").join("policy.json");
        let env = env_from(&[
            (POLICY_FINGERPRINT_ENV, "stale-fingerprint"),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);
        let current = SafetyPolicy::default();

        let first = evaluate_with_attestation_evidence(
            &current,
            "cargo test",
            LaunchMode::Headless,
            &|k| env.get(k).cloned(),
        );
        assert_eq!(first.status, "self-healed");
        assert!(snapshot.exists(), "the snapshot file must be rewritten");

        let rewritten: SafetyPolicy =
            serde_json::from_str(&std::fs::read_to_string(&snapshot).expect("read"))
                .expect("valid policy JSON");
        assert_eq!(rewritten, current);
    }

    /// Missing exactly one of the two attestation env vars is the same
    /// "invalid" shape as a corrupt file -- it must self-heal too, not fall
    /// through to some third behavior.
    #[test]
    fn a_partial_attestation_pair_self_heals() {
        let env = env_from(&[(POLICY_FINGERPRINT_ENV, "some-fingerprint")]);
        let current = SafetyPolicy::default();
        let evidence = evaluate_with_attestation_evidence(
            &current,
            "cargo test",
            LaunchMode::Interactive,
            &|k| env.get(k).cloned(),
        );
        assert_eq!(evidence.status, "self-healed");
        assert_eq!(evidence.outcome.verdict, Verdict::Allow);
    }
```

Keep `evaluate_with_attestation_evidence_reports_snapshot_stricter_when_the_current_policy_widened` (line ~4377) and `evaluate_with_attestation_evidence_reports_unchanged_when_the_snapshot_agrees` (line ~4420) exactly as they are — both already exercise the VALID-snapshot path, which this task does not touch, and together they are the "divergent-but-valid snapshot still keeps the stricter verdict" regression this design decision asks to keep.

- [ ] **Step 2: Run to confirm the new/rewritten tests fail**

Run: `cargo test --lib commands::ctx::safety::tests -- --test-threads=8 self_heal a_tampered a_partial_attestation`
Expected: FAIL (old blanket-deny/ask behavior still in place; `self_healed_evaluation` does not exist yet)

- [ ] **Step 3: Implement self-heal**

Add above `evaluate_with_attestation_evidence` in `src/commands/ctx/safety.rs`:

```rust
/// Issue #168, design decision (c): what an invalid attestation snapshot
/// (absent one of the two env vars, an unreadable/unparseable file, or a
/// hash mismatch) now produces INSTEAD of the old blanket `attestation_
/// failure(mode)` (interactive `Ask`/headless `Deny` on every single
/// command for the rest of the session, with no way out short of a
/// restart). A broken snapshot proves nothing about `current` -- the
/// in-process policy this same launch already resolved from `~/.zirv/
/// ctx.toml` and any repo `.zirv/ctx.toml` -- so this falls back to
/// evaluating `current` alone, exactly like the "no attestation configured
/// at all" case, and best-effort re-materializes the snapshot file at
/// `snapshot_path` (when one was named) so the NEXT command in this same
/// session attests cleanly again instead of re-detecting the identical
/// broken file every time. The re-materialization write failing is
/// silently ignored: it only ever improves the next call, never gates this
/// one. `status: "self-healed"` distinguishes this path in the audit log
/// and from both `"not-present"` and `"valid"`.
fn self_healed_evaluation(
    current: &SafetyPolicy,
    command: &str,
    mode: super::adapters::LaunchMode,
    current_fingerprint: String,
    launch_fingerprint: Option<String>,
    snapshot_path: Option<&str>,
) -> AttestedEvaluation {
    if let Some(path) = snapshot_path {
        let _ = rematerialize_policy_snapshot(path, current);
    }
    AttestedEvaluation {
        outcome: evaluate(current, command, mode),
        current_fingerprint,
        launch_fingerprint,
        status: "self-healed",
        divergence: SnapshotDivergence::Unchanged,
    }
}

/// Best-effort rewrite of the policy snapshot file at `path` from `policy` --
/// the identical body `adapters::claude::launch_settings_path` writes at
/// launch, reused here (via the same pretty-JSON-plus-trailing-newline
/// shape) so a self-heal and a fresh launch can never format the snapshot
/// two different ways. Errors are the caller's to ignore: this is a repair
/// attempt for the NEXT command, never a gate on the current one.
fn rematerialize_policy_snapshot(path: &str, policy: &SafetyPolicy) -> std::io::Result<()> {
    let mut body = serde_json::to_string_pretty(policy).map_err(std::io::Error::other)?;
    body.push('\n');
    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        super::state::create_private_dir_all(parent)?;
    }
    super::state::write_private(path, &body)
}
```

Then change `evaluate_with_attestation_evidence`'s two invalid-shape branches. Replace:

```rust
            (fingerprint, _) => {
                return AttestedEvaluation {
                    outcome: attestation_failure(mode),
                    current_fingerprint,
                    launch_fingerprint: fingerprint,
                    status: "invalid",
                    divergence: SnapshotDivergence::Unchanged,
                };
            }
        };

    let launch = std::fs::read_to_string(snapshot_path)
        .ok()
        .and_then(|body| serde_json::from_str::<SafetyPolicy>(&body).ok());
    let Some(launch) = launch else {
        return AttestedEvaluation {
            outcome: attestation_failure(mode),
            current_fingerprint,
            launch_fingerprint: Some(expected_fingerprint),
            status: "invalid",
            divergence: SnapshotDivergence::Unchanged,
        };
    };
    if policy_fingerprint(&launch).ok().as_deref() != Some(expected_fingerprint.as_str()) {
        return AttestedEvaluation {
            outcome: attestation_failure(mode),
            current_fingerprint,
            launch_fingerprint: Some(expected_fingerprint),
            status: "invalid",
            divergence: SnapshotDivergence::Unchanged,
        };
    }
```

with:

```rust
            (fingerprint, _) => {
                return self_healed_evaluation(
                    current,
                    command,
                    mode,
                    current_fingerprint,
                    fingerprint,
                    None,
                );
            }
        };

    let launch = std::fs::read_to_string(&snapshot_path)
        .ok()
        .and_then(|body| serde_json::from_str::<SafetyPolicy>(&body).ok());
    let Some(launch) = launch else {
        return self_healed_evaluation(
            current,
            command,
            mode,
            current_fingerprint,
            Some(expected_fingerprint),
            Some(snapshot_path.as_str()),
        );
    };
    if policy_fingerprint(&launch).ok().as_deref() != Some(expected_fingerprint.as_str()) {
        return self_healed_evaluation(
            current,
            command,
            mode,
            current_fingerprint,
            Some(expected_fingerprint),
            Some(snapshot_path.as_str()),
        );
    }
```

`attestation_failure` now has no remaining callers inside this function; leave the function itself in place (still referenced by doc comments and, if `cargo clippy -D warnings` flags it as dead code, add `#[allow(dead_code)]` with a one-line comment noting it is kept as the pre-#168 behavior's documentation anchor — but check first, since `run_check_hook_mode_with_env`'s doc comments still reference it by name only, not by call).

- [ ] **Step 4: Run the attestation tests**

Run: `cargo test --lib commands::ctx::safety::tests -- --test-threads=8 self_heal a_tampered a_partial_attestation evaluate_with_attestation`
Expected: PASS

- [ ] **Step 5: Run the full safety module and check for the now-possibly-unused `attestation_failure`**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: PASS. If clippy flags `attestation_failure` as dead code, add `#[allow(dead_code)]` directly above its `fn` with a short comment: `// Issue #168: no longer called (self-heal replaced its use in evaluate_with_attestation_evidence) -- kept as the documented pre-#168 shape referenced by nearby doc comments.`

- [ ] **Step 6: Run the other four verify commands**

Run: `cargo build`, `cargo nextest run --no-fail-fast -j 8`, `cargo test --verbose -- --test-threads=8`, `cargo fmt -- --check`
Expected: all pass

- [ ] **Step 7: Commit**

```bash
git add src/commands/ctx/safety.rs
git commit -m "feat(ctx-safety): self-heal an invalid attestation snapshot instead of ask/deny-everything"
```

---

## Task 5: `cd <known-root> && <cmd>` classifies by `<cmd>` (decision e)

**Files:**
- Modify: `src/commands/ctx/safety.rs` (new function near `run_check_hook_mode_with_env`; wiring at the top of that function)
- Test: `src/commands/ctx/safety.rs` (`mod tests`)

**Interfaces:**
- Consumes: `strip_quotes` (pre-existing, private in this module).
- Produces: `fn strip_known_root_cd_prefix(command: &str, allowed_roots: &[String]) -> Option<String>` and `fn cd_allow_roots(scratchpad_root: &str) -> Vec<String>`.

- [ ] **Step 1: Write the failing unit tests for the pure prefix-stripping function**

```rust
    // -- strip_known_root_cd_prefix (issue #168, decision e) --------------

    #[test]
    fn cd_prefix_strips_a_known_worktree_or_scratchpad_root() {
        let roots = vec!["/repo".to_string(), "/tmp/claude".to_string()];
        assert_eq!(
            strip_known_root_cd_prefix("cd /repo && git log", &roots).as_deref(),
            Some("git log")
        );
        assert_eq!(
            strip_known_root_cd_prefix("cd /repo/sub && git log", &roots).as_deref(),
            Some("git log")
        );
        assert_eq!(
            strip_known_root_cd_prefix("cd /tmp/claude/out; ls", &roots).as_deref(),
            Some("ls")
        );
        assert_eq!(
            strip_known_root_cd_prefix(
                "cd /anywhere/.claude/worktrees/feat && cargo fmt",
                &roots
            )
            .as_deref(),
            Some("cargo fmt")
        );
    }

    #[test]
    fn cd_prefix_leaves_unknown_or_dynamic_paths_untouched() {
        let roots = vec!["/repo".to_string()];
        for command in [
            "cd /etc && rm -rf .",
            "cd $HOME/evil && rm -rf .",
            "cd `pwd`/x && rm -rf .",
            "cd ~ && rm -rf .",
            "cd /repo",
            "cd /repo/../../etc && cat shadow",
        ] {
            assert!(
                strip_known_root_cd_prefix(command, &roots).is_none(),
                "{command} must not be stripped"
            );
        }
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib commands::ctx::safety::tests::cd_prefix -- --test-threads=8`
Expected: compile error, function not found.

- [ ] **Step 3: Implement**

Add above `run_check_hook_mode_with_env` in `src/commands/ctx/safety.rs`:

```rust
/// Issue #168, design decision (e): if `command` begins with a literal (no
/// `$`, backtick, `~`, or glob character), single-token `cd <path>` segment
/// followed by `&&`, `;`, or a newline, and `<path>` resolves under one of
/// `allowed_roots` OR contains a `.claude/worktrees` path component anywhere,
/// returns the remainder with that leading segment stripped -- e.g. `cd
/// <worktree> && git log` becomes `git log`, so the compound is classified
/// by the real work alone instead of the unmatched-by-any-rule `cd` segment
/// dragging the whole thing to the mode default. `None` leaves `command`
/// untouched: no leading `cd` at all, an unproven/dynamic path, a `cd`
/// containing `..` (this classifier is text-only and cannot re-resolve a
/// relative escape), or a bare `cd <path>` with nothing chained after it
/// (left to classify exactly as it does today).
pub(crate) fn strip_known_root_cd_prefix(command: &str, allowed_roots: &[String]) -> Option<String> {
    let trimmed = command.trim_start();
    let rest = trimmed.strip_prefix("cd ")?;
    let (split_at, sep_len) = ["&&", ";", "\n"]
        .iter()
        .filter_map(|sep| rest.find(sep).map(|idx| (idx, sep.len())))
        .min_by_key(|&(idx, _)| idx)?;
    let (path_token, remainder) = {
        let (head, tail) = rest.split_at(split_at);
        (head.trim(), tail[sep_len..].trim())
    };
    if path_token.is_empty() || remainder.is_empty() {
        return None;
    }
    if path_token.split_whitespace().count() != 1
        || path_token.contains(['$', '`', '~', '*', '?'])
        || path_token.contains("..")
    {
        return None;
    }
    let normalized = strip_quotes(path_token).replace('\\', "/");
    let under_worktrees = normalized.contains(".claude/worktrees/") || normalized.ends_with(".claude/worktrees");
    let under_allowed_root = allowed_roots.iter().any(|root| {
        !root.is_empty() && (normalized == *root || normalized.starts_with(&format!("{root}/")))
    });
    if under_worktrees || under_allowed_root {
        Some(remainder.to_string())
    } else {
        None
    }
}

/// The roots [`strip_known_root_cd_prefix`] treats as known-safe `cd`
/// targets: the session scratchpad, and this process's own working
/// directory (the repo root, for a hook process launched from inside it).
fn cd_allow_roots(scratchpad_root: &str) -> Vec<String> {
    let mut roots = vec![scratchpad_root.to_string()];
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(
            cwd.to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_string(),
        );
    }
    roots
}
```

- [ ] **Step 4: Run the unit tests**

Run: `cargo test --lib commands::ctx::safety::tests::cd_prefix -- --test-threads=8`
Expected: PASS

- [ ] **Step 5: Wire into `run_check_hook_mode_with_env`**

At the top of `run_check_hook_mode_with_env`, right after `mode` is computed and before `evaluate_with_attestation_evidence` is called, insert:

```rust
    // Issue #168, design decision (e): a leading, literal `cd <known-root>`
    // prefix is classified away so `cd <worktree> && git log` is judged by
    // `git log` alone. `command` (the ORIGINAL, unstripped text) is still
    // what reaches `hook_output`/`audit_hook_decision` below, so the log and
    // any denial message always name what was actually run.
    let scratchpad_roots = vec![scratchpad_write_root(&std::env::temp_dir())];
    let cd_roots = cd_allow_roots(&scratchpad_roots[0]);
    let effective_command =
        strip_known_root_cd_prefix(command, &cd_roots).unwrap_or_else(|| command.to_string());

    let evidence = evaluate_with_attestation_evidence(&cfg.safety, &effective_command, mode, env);
    let mut outcome = evidence.outcome.clone();
```

This replaces the two lines:
```rust
    let evidence = evaluate_with_attestation_evidence(&cfg.safety, command, mode, env);
    let mut outcome = evidence.outcome.clone();
```

Note this also removes the `let scratchpad_roots = vec![...]` binding Task 2 added right before the retry `if` -- it is now computed here instead, once, earlier in the function. Delete Task 2's duplicate binding at the retry-branch call site. Every remaining `command` reference inside the `dangerously_disable_sandbox` branch (`is_sandbox_bypass_safe_gh_command(command)`, `is_zirv_ctx_escape_safe(command)`, `is_read_only_escape_safe(command, &scratchpad_roots)`, `escape_allow_matches(&cfg.safety.escape_allow, command)`) must be changed to `&effective_command` so the stripped form is what gets classified end to end; `hook_output(command, ...)` and `audit_hook_decision(&payload, command, ...)` keep the ORIGINAL `command`.

- [ ] **Step 6: Write the end-to-end hook-mode test**

```rust
    #[test]
    fn a_cd_into_the_process_working_directory_then_git_log_allows_headlessly() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let cwd = std::env::current_dir().expect("cwd").to_string_lossy().replace('\\', "/");
        let command = format!("cd {cwd} && git log");
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}"}},"permission_mode":"dontAsk"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains(r#""permissionDecision":"ask""#) && !text.contains("deny"),
            "cd into the process cwd then a plain git log must classify by git log alone: got {text}"
        );
    }

    #[test]
    fn a_cd_into_an_unknown_root_then_a_destructive_command_still_escalates() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"cd /etc && rm -rf ."},"permission_mode":"dontAsk"}"#;
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""permissionDecision":"ask""#),
            "an unknown-root cd ahead of rm -rf must still ask: got {text}"
        );
    }
```

- [ ] **Step 7: Run the full safety module**

Run: `cargo test --lib commands::ctx::safety:: -- --test-threads=8`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add src/commands/ctx/safety.rs
git commit -m "feat(ctx-safety): classify a leading cd <known-root> prefix by the command after it"
```

---

## Task 6: Scratchpad-confined-write carve-out (decision d)

**Files:**
- Modify: `src/commands/ctx/safety.rs` (new functions near Task 2's `target_is_confined`; wiring in `run_check_hook_mode_with_env`)
- Test: `src/commands/ctx/safety.rs` (`mod tests`)

**Interfaces:**
- Consumes: `redact_single_quoted_heredocs`, `split_segments`, `sql_tokens`, `sql_program_name`, `collapse_whitespace`, `target_is_confined` (Task 2), `evaluate_candidate_outcome` (Task 1), `normalize_segments`.
- Produces: `fn write_targets_confined(command: &str, scratchpad_roots: &[String]) -> Option<bool>`, `fn every_segment_is_allow_or_unmatched_default(policy: &SafetyPolicy, command: &str, fallback: Verdict) -> bool`.

- [ ] **Step 1: Write the failing tests for the redirection-target scanner**

```rust
    // -- write_targets_confined (issue #168, decision d) ------------------

    #[test]
    fn write_targets_confined_allows_dev_null_and_scratchpad_targets() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in [
            "echo hi > /dev/null",
            "some-tool --flag > /tmp/claude/out.log",
            "some-tool --flag >> /tmp/claude/out.log",
            "some-tool 2> /tmp/claude/err.log",
            "some-tool | tee /tmp/claude/combined.log",
            "some-tool | tee -a /tmp/claude/combined.log",
            "git log && echo done > /tmp/claude/marker",
        ] {
            assert_eq!(
                write_targets_confined(command, &roots),
                Some(true),
                "{command}"
            );
        }
    }

    /// CRITICAL regression lock: a command with NO write target at all --
    /// no redirection, or one that resolves to descriptor duplication only
    /// (`2>&1`, no path) -- must never vacuously satisfy "every target is
    /// confined". Without this, an arbitrary unmatched command with zero
    /// writes (`kubectl exec -it pod -- sh`) would be silently widened to
    /// `Allow` by the caller just for not writing anywhere at all.
    #[test]
    fn write_targets_confined_has_no_opinion_when_there_is_no_write_target_at_all() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in ["kubectl exec -it pod -- sh", "some-tool 2>&1", "git log"] {
            assert_eq!(write_targets_confined(command, &roots), None, "{command}");
        }
    }

    #[test]
    fn write_targets_confined_rejects_targets_outside_the_scratchpad() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in [
            "echo hi > /etc/passwd",
            "some-tool >> ~/.bashrc",
            "some-tool | tee /etc/shadow",
        ] {
            assert_eq!(
                write_targets_confined(command, &roots),
                Some(false),
                "{command}"
            );
        }
    }

    #[test]
    fn write_targets_confined_has_no_opinion_on_unparseable_targets() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in ["some-tool > $OUT", "some-tool > \"$(mktemp)\""] {
            assert_eq!(write_targets_confined(command, &roots), None, "{command}");
        }
    }

    #[test]
    fn write_targets_confined_returns_none_with_no_scratchpad_roots_configured() {
        assert_eq!(write_targets_confined("echo hi > /dev/null", &[]), None);
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib commands::ctx::safety::tests::write_targets_confined -- --test-threads=8`
Expected: compile error, function not found.

- [ ] **Step 3: Implement the scanner and confinement check**

Add next to `target_is_confined` (introduced in Task 2) in `src/commands/ctx/safety.rs`:

```rust
/// Issue #168, design decision (d): scans `segment` (already heredoc-
/// redacted by the caller) for unquoted output-redirection targets -- `>`,
/// `>>`, `<`, digit-prefixed (`1>`/`2>`) and `&>`/`&>>` forms, with or
/// without a space before the target text. `None` means a redirection
/// operator was found with NOTHING usable after it (a dangling operator at
/// the very end of the segment) -- ambiguous, never guessed at. `&1`/`&2`
/// (descriptor duplication, e.g. `2>&1`) names no filesystem path at all and
/// is recognized and skipped, not treated as ambiguous: this is a character-
/// level scan (unlike a whitespace-token split), so it cannot miss a glued
/// operator the way a token-based scan could, and every quote/escape rule
/// mirrors [`contains_unquoted_redirection`] so the two can never disagree
/// about which `>`/`<` in the text is live shell syntax versus quoted data.
fn scan_redirection_targets(segment: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = segment.chars().collect();
    let mut targets = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            i += 1;
            continue;
        }
        if let Some(active) = quote {
            if c == active {
                quote = None;
            }
            i += 1;
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
            i += 1;
            continue;
        }
        let is_digit_prefixed_redirect = c.is_ascii_digit() && chars.get(i + 1) == Some(&'>');
        let is_amp_redirect = c == '&' && chars.get(i + 1) == Some(&'>');
        if !(c == '>' || c == '<' || is_amp_redirect || is_digit_prefixed_redirect) {
            i += 1;
            continue;
        }
        let mut j = if is_amp_redirect || is_digit_prefixed_redirect {
            i + 2
        } else {
            i + 1
        };
        if chars.get(j) == Some(&'>') {
            j += 1;
        }
        while chars.get(j) == Some(&' ') {
            j += 1;
        }
        let target_start = j;
        while j < chars.len() && !chars[j].is_whitespace() && chars[j] != '>' && chars[j] != '<' {
            j += 1;
        }
        let target: String = chars[target_start..j].iter().collect();
        if target.is_empty() {
            // A dangling operator with nothing after it at all -- ambiguous.
            return None;
        }
        if !matches!(target.as_str(), "&1" | "&2") {
            targets.push(target);
        }
        i = j;
    }
    Some(targets)
}

/// Issue #168, design decision (d): one segment's write targets, or `None`
/// if this cannot be confidently resolved -- either a dangling redirection
/// operator ([`scan_redirection_targets`] itself), or a `tee` argument
/// containing `$`/backtick so it cannot be proven a literal path.
fn segment_write_targets(segment: &str) -> Option<Vec<String>> {
    let mut targets = scan_redirection_targets(segment)?;
    if let Some(tokens) = sql_tokens(&collapse_whitespace(segment))
        && let Some(first) = tokens.first()
        && sql_program_name(first) == "tee"
    {
        for token in tokens.iter().skip(1) {
            if token.starts_with('-') {
                continue;
            }
            if token.contains(['$', '`']) {
                return None;
            }
            targets.push(token.clone());
        }
    }
    Some(targets)
}

/// Issue #168, design decision (d): whether every write target across every
/// segment of `command` is `/dev/null` or beneath one of `scratchpad_roots`.
/// `None` -- no opinion, exactly today's un-analyzed behavior -- whenever
/// `scratchpad_roots` is empty, any segment's own targets cannot be
/// confidently resolved (see [`segment_write_targets`]), a target contains
/// `$`/backtick (built through substitution/expansion this text-only module
/// cannot resolve -- distinct from a target merely containing `~`/a glob
/// character, which [`target_is_confined`] can confidently call "not
/// confined" without further ambiguity), or -- CRITICAL -- `command` names
/// no write target at all (no redirection, no `tee`). That last case matters
/// because this function's caller only ever widens a verdict when it
/// returns `Some(true)`: without this guard, ANY command with zero writes
/// (an ordinary `kubectl exec -it pod -- sh`, a bare `2>&1` with nothing
/// else) would vacuously satisfy "every target is confined" and get widened
/// to `Allow` just for not writing anywhere -- which is not what this design
/// decision is for (a compound that DOES write, confined to the
/// scratchpad). `None` here correctly leaves such a command to classify
/// exactly as it does today. Heredoc bodies are redacted first, the same as
/// every other classifier in this module.
pub(crate) fn write_targets_confined(command: &str, scratchpad_roots: &[String]) -> Option<bool> {
    if scratchpad_roots.is_empty() {
        return None;
    }
    let sanitized = redact_single_quoted_heredocs(command);
    let mut confined = true;
    let mut saw_any_target = false;
    for segment in split_segments(&sanitized) {
        let targets = segment_write_targets(&segment)?;
        for target in &targets {
            saw_any_target = true;
            if target.contains(['$', '`']) {
                return None;
            }
            if !target_is_confined(target, scratchpad_roots) {
                confined = false;
            }
        }
    }
    if !saw_any_target {
        return None;
    }
    Some(confined)
}

/// Issue #168, design decision (d): true when every one of `command`'s
/// normalized executable candidates evaluates to `Allow`, or to the plain,
/// no-rule-matched mode default -- the check [`write_targets_confined`]'s
/// caller needs before it will widen a compound's default `Ask` to `Allow`:
/// an explicit operator/repo `ask` rule, or any `deny` rule, naming one
/// segment must still win, never be silently overridden just because that
/// segment also happens to write somewhere confined.
fn every_segment_is_allow_or_unmatched_default(
    policy: &SafetyPolicy,
    command: &str,
    fallback: Verdict,
) -> bool {
    let candidates = normalize_segments(command);
    if candidates.is_empty() {
        return false;
    }
    candidates.iter().all(|candidate| {
        let outcome = evaluate_candidate_outcome(policy, candidate, fallback);
        outcome.verdict == Verdict::Allow || (outcome.verdict == fallback && outcome.matched.is_none())
    })
}
```

- [ ] **Step 4: Run the scanner/confinement tests**

Run: `cargo test --lib commands::ctx::safety::tests::write_targets_confined -- --test-threads=8`
Expected: PASS

- [ ] **Step 5: Wire into `run_check_hook_mode_with_env`, applied on both the ordinary and retry paths**

Immediately after the `let mut outcome = evidence.outcome.clone();` line (now reading `effective_command` per Task 5), and BEFORE the `dangerously_disable_sandbox` branch, insert:

```rust
    // Issue #168, design decision (d): a compound whose every write target
    // is confined to the session scratchpad is treated as `Allow` even when
    // it would otherwise rely on the unmatched-command mode default. Checked
    // BEFORE the sandbox-retry branch below so the same widening survives an
    // unsandboxed retry too (see that branch's `already_scratchpad_confined`
    // short-circuit).
    if outcome.verdict != Verdict::Deny
        && every_segment_is_allow_or_unmatched_default(
            &cfg.safety,
            &effective_command,
            cfg.safety.default_verdict(mode),
        )
        && write_targets_confined(&effective_command, &scratchpad_roots) == Some(true)
    {
        outcome = Outcome {
            verdict: Verdict::Allow,
            matched: Some(Rule {
                pattern: "<scratchpad: confined write>".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
    }
```

Then, inside the `dangerously_disable_sandbox` branch, make the widened `Allow` above survive the retry instead of being degraded. Change:

```rust
    if payload.tool_input.dangerously_disable_sandbox && outcome.verdict != Verdict::Deny {
        outcome = if outcome.verdict == Verdict::Allow && is_sandbox_bypass_safe_gh_command(&effective_command)
        {
```

to:

```rust
    if payload.tool_input.dangerously_disable_sandbox && outcome.verdict != Verdict::Deny {
        let already_scratchpad_confined = outcome
            .matched
            .as_ref()
            .is_some_and(|rule| rule.pattern == "<scratchpad: confined write>");
        outcome = if already_scratchpad_confined {
            outcome
        } else if outcome.verdict == Verdict::Allow
            && is_sandbox_bypass_safe_gh_command(&effective_command)
        {
```

(every other arm in that `if`/`else if` chain, and its final `else { degrade }`, is unchanged).

- [ ] **Step 6: Write end-to-end hook-mode tests**

```rust
    #[test]
    fn a_scratchpad_confined_compound_allows_headlessly_even_unmatched() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let scratchpad = scratchpad_write_root(&std::env::temp_dir());
        let command = format!("some-totally-unknown-tool --flag > {scratchpad}/out.log");
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}"}},"permission_mode":"dontAsk"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains(r#""permissionDecision":"ask""#) && !text.contains("deny"),
            "a scratchpad-confined write from an otherwise-unmatched command must not prompt: got {text}"
        );
    }

    #[test]
    fn a_scratchpad_confined_compound_survives_an_unsandboxed_retry() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let scratchpad = scratchpad_write_root(&std::env::temp_dir());
        let command = format!("grep -r TODO . > {scratchpad}/todos.log");
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":true}},"permission_mode":"default"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""permissionDecision":"allow""#),
            "got {text}"
        );
        assert!(!text.contains("unsandboxed retry"), "got {text}");
    }

    #[test]
    fn a_write_outside_the_scratchpad_still_escalates_even_if_otherwise_unmatched() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"some-totally-unknown-tool > /etc/passwd"},"permission_mode":"dontAsk"}"#;
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""permissionDecision":"ask""#),
            "got {text}"
        );
    }
```

- [ ] **Step 7: Run the full safety module and the other four verify commands**

Run: `cargo test --lib commands::ctx::safety:: -- --test-threads=8`
Expected: PASS
Run: `cargo build`, `cargo nextest run --no-fail-fast -j 8`, `cargo test --verbose -- --test-threads=8`, `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`
Expected: all pass

- [ ] **Step 8: Commit**

```bash
git add src/commands/ctx/safety.rs
git commit -m "feat(ctx-safety): allow compounds whose writes are confined to the session scratchpad"
```

---

## Task 7: Extend the sandbox write allowlist to zirv's own state dirs (decision g)

**Files:**
- Modify: `src/commands/ctx/adapters/claude.rs:494-536` (`launch_settings_path`), `:561-599` (`launch_settings_value`), and its five existing test call sites (lines ~1423, ~2149, ~2195, ~2213, ~2272 — re-check exact line numbers in the worktree before editing, this file may have shifted slightly from earlier tasks' edits to `safety.rs`, a sibling file, which does not itself change these line numbers, but confirm before editing)
- Test: `src/commands/ctx/adapters/claude.rs` (`mod tests`)

**Interfaces:**
- Changes the signature of `fn launch_settings_value(safety: &SafetyPolicy, policy_path: &Path, mail_write_dir: Option<&Path>) -> Result<Value, serde_json::Error>` to `fn launch_settings_value(safety: &SafetyPolicy, policy_path: &Path, allow_write_dirs: &[PathBuf]) -> Result<Value, serde_json::Error>`. Every caller in this file must be updated in the same commit.

- [ ] **Step 1: Write the failing test for the widened allowlist**

Replace the existing `launch_settings_allow_write_to_the_mail_dir_but_never_the_policy_snapshot_dir` test (around line 2190) with:

```rust
    /// Issue #147 (mail) extended by issue #168: every one of zirv's own
    /// state-dir writes a sandboxed session legitimately needs -- mail,
    /// memory, logs (the safety audit log itself), groups, and handoffs --
    /// is allow-listed, but the policy-snapshot/attestation directory
    /// sitting alongside them (under the operator's HOME, not the state
    /// root) never is.
    #[cfg(not(windows))]
    #[test]
    fn launch_settings_allow_write_to_every_zirv_state_dir_but_never_the_policy_snapshot_dir() {
        let policy = super::super::super::safety::SafetyPolicy::default();
        let policy_path = Path::new("/home/op/.zirv/runtime/policies/abc123.json");
        let dirs = vec![
            PathBuf::from("/state/mail"),
            PathBuf::from("/state/memory"),
            PathBuf::from("/state/logs"),
            PathBuf::from("/state/groups"),
            PathBuf::from("/state/handoffs"),
        ];
        let settings = launch_settings_value(&policy, policy_path, &dirs).expect("settings");
        let allow_write = settings["sandbox"]["filesystem"]["allowWrite"]
            .as_array()
            .expect("allowWrite must be present when dirs are given");
        for expected in ["/state/mail", "/state/memory", "/state/logs", "/state/groups", "/state/handoffs"] {
            assert!(
                allow_write.iter().any(|entry| entry == expected),
                "{expected} must be allow-listed for write: {settings}"
            );
        }
        assert!(
            !allow_write
                .iter()
                .any(|entry| entry.as_str().is_some_and(|s| s.contains("runtime/policies"))),
            "the policy-snapshot/attestation dir must never be allow-listed for write: {settings}"
        );

        // No dirs resolved (best-effort failure): no allowWrite key at all,
        // never an empty-but-present one that could mask a future bug.
        let settings_without_dirs =
            launch_settings_value(&policy, policy_path, &[]).expect("settings");
        assert!(settings_without_dirs["sandbox"]["filesystem"]["allowWrite"].is_null());
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib commands::ctx::adapters::claude::tests::launch_settings_allow_write_to_every_zirv_state_dir -- --test-threads=8`
Expected: compile error (signature mismatch) on non-Windows; on this Windows machine the `#[cfg(not(windows))]` test does not even compile in, so instead run:
Run: `cargo build`
Expected: no error yet (the test body is cfg'd out on Windows, but the signature-changing edit below has not been made yet, so this step's real confirmation happens after Step 3 below, on Linux/Docker — see Step 6). On Windows, proceed straight to Step 3; the compile-time signature change itself is the thing this step's edits actually verify here.

- [ ] **Step 3: Change `launch_settings_value`'s signature and body**

In `src/commands/ctx/adapters/claude.rs`, change:

```rust
fn launch_settings_value(
    safety: &super::super::safety::SafetyPolicy,
    policy_path: &Path,
    mail_write_dir: Option<&Path>,
) -> Result<Value, serde_json::Error> {
```

to:

```rust
fn launch_settings_value(
    safety: &super::super::safety::SafetyPolicy,
    policy_path: &Path,
    allow_write_dirs: &[PathBuf],
) -> Result<Value, serde_json::Error> {
```

and change the body:

```rust
        if let Some(mail_dir) = mail_write_dir {
            filesystem["allowWrite"] = serde_json::json!([mail_dir.display().to_string()]);
        }
```

to:

```rust
        if !allow_write_dirs.is_empty() {
            filesystem["allowWrite"] = serde_json::json!(
                allow_write_dirs
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>()
            );
        }
```

- [ ] **Step 4: Update `launch_settings_path` (the production caller) and every test call site**

In `launch_settings_path`, change:

```rust
        let mail_dir =
            super::super::state::StateDir::resolve(&super::super::config::env_from_process())
                .ok()
                .map(|state| state.mail());
```

to:

```rust
        // Issue #168: zirv's own state dirs join the mailbox on the sandbox
        // write allowlist -- memory (cross-session memory bank writes), logs
        // (the safety audit log itself), groups (work-group records), and
        // handoffs (ctx handover files) all need write access from inside a
        // sandboxed session, exactly like mail already did (issue #147).
        let allow_write_dirs: Vec<PathBuf> =
            super::super::state::StateDir::resolve(&super::super::config::env_from_process())
                .ok()
                .map(|state| {
                    vec![
                        state.mail(),
                        state.memory(),
                        state.logs(),
                        state.groups(),
                        state.handoffs(),
                    ]
                })
                .unwrap_or_default();
```

and change:

```rust
            let settings = launch_settings_value(safety, &policy_path, mail_dir.as_deref())
                .map_err(std::io::Error::other)?;
```

to:

```rust
            let settings = launch_settings_value(safety, &policy_path, &allow_write_dirs)
                .map_err(std::io::Error::other)?;
```

Update the four remaining test call sites:
- `test_launch_settings()` (~line 1423): `launch_settings_value(&Default::default(), Path::new("zirv-test-safety-policy.json"), &[])`
- `launch_settings_bind_the_hook_to_an_immutable_policy_snapshot` (~line 2149): `launch_settings_value(&policy, policy_path, &[])`
- the tail of `launch_settings_are_materialized_atomically_under_the_zirv_home` (~line 2265-2272): replace

```rust
        let mail_dir = super::super::super::state::StateDir::resolve(
            &super::super::super::config::env_from_process(),
        )
        .ok()
        .map(|state| state.mail());
        assert_eq!(
            written,
            launch_settings_value(&policy, &policy_path, mail_dir.as_deref()).expect("settings")
        );
```

with:

```rust
        let allow_write_dirs: Vec<PathBuf> = super::super::super::state::StateDir::resolve(
            &super::super::super::config::env_from_process(),
        )
        .ok()
        .map(|state| {
            vec![
                state.mail(),
                state.memory(),
                state.logs(),
                state.groups(),
                state.handoffs(),
            ]
        })
        .unwrap_or_default();
        assert_eq!(
            written,
            launch_settings_value(&policy, &policy_path, &allow_write_dirs).expect("settings")
        );
```

- [ ] **Step 5: Build and run this file's own tests**

Run: `cargo build`
Expected: builds clean (the `#[cfg(not(windows))]` tests are compiled out on this machine, but every non-cfg'd call site must still type-check)
Run: `cargo test --lib commands::ctx::adapters::claude:: -- --test-threads=8`
Expected: PASS (the Windows-runnable subset)

- [ ] **Step 6: Linux/Docker verification (required for this file per repo convention)**

This edit lives inside `#[cfg(not(windows))]` code and its tests, which never compile or run on this Windows machine. Per `CLAUDE.md`'s own convention for `wrap`/`announce`/`pace`/adapter-argv changes, verify this specific change on Linux before considering the task done:

```bash
git -c core.autocrlf=false archive HEAD | (mkdir -p /tmp/zirv-168-verify && tar -x -C /tmp/zirv-168-verify)
```

then, inside a `rust:1-bookworm` container as a non-root user, in `/tmp/zirv-168-verify`:

```bash
cargo test --bin zirv commands::ctx::adapters::claude:: -- --test-threads=8
cargo clippy --all-targets -- -D warnings
```

Expected: PASS on both. Record the result in the task's commit message trailer (e.g. `Verified on rust:1-bookworm (non-root): cargo test + clippy both green.`) since this machine cannot re-run this check itself.

- [ ] **Step 7: Commit**

```bash
git add src/commands/ctx/adapters/claude.rs
git commit -m "feat(ctx-adapters): allow-list zirv's own state dirs for sandboxed writes, not just mail"
```

---

## Task 8: Regression-lock that read-only find/locate/rg never hard-deny (decision f)

**Files:**
- Test only: `src/commands/ctx/safety.rs` (`mod tests`)

**Interfaces:**
- No new production code. Pure regression test using `evaluate` and `run_check_hook_mode` (both pre-existing).

- [ ] **Step 1: Write the regression-lock test**

```rust
    // -- issue #168, decision (f): find/locate/rg never hard-deny --------

    /// No policy layer -- built-in, and none of this task's new classifiers
    /// -- ever produces `Deny` for an ordinary read-only `find`/`locate`/
    /// `rg` invocation, across both launch modes, both permission modes
    /// hook-mode cares about, and both values of `dangerouslyDisableSandbox`.
    /// `find -exec`/`-ok` running something unproven still legitimately
    /// escalates to `Ask` (`is_risky_find_exec`) -- this only pins the
    /// FLOOR, never `Deny`, for the read-only shapes below.
    #[test]
    fn read_only_find_locate_rg_never_hard_deny_via_plain_evaluate() {
        let policy = SafetyPolicy::default();
        for command in [
            "find . -name '*.rs'",
            "find ./src -iname '*.md' -type f",
            "find / -name id_rsa",
            "find ~ -name '*.pem'",
            "locate id_rsa",
            "locate -i '*.env'",
            "rg TODO .",
            "rg --hidden -i password .",
            "find . -exec grep -l TODO {} +",
            "find . -exec sed -n '1p' {} +",
        ] {
            for mode in [LaunchMode::Interactive, LaunchMode::Headless] {
                let outcome = evaluate(&policy, command, mode);
                assert_ne!(
                    outcome.verdict,
                    Verdict::Deny,
                    "{command} ({mode:?}) must never hard-deny: {outcome:?}"
                );
            }
        }
    }

    #[test]
    fn read_only_find_locate_rg_never_hard_deny_through_the_hook_either() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        for command in [
            "find . -name '*.rs'",
            "find / -name id_rsa",
            "locate id_rsa",
            "rg TODO .",
        ] {
            for permission_mode in ["default", "dontAsk"] {
                for dangerously_disable_sandbox in [true, false] {
                    let stdin = format!(
                        r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":{dangerously_disable_sandbox}}},"permission_mode":"{permission_mode}"}}"#
                    );
                    let mut out = Vec::new();
                    run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
                    let text = String::from_utf8(out).expect("utf8");
                    assert!(
                        !text.contains(r#""permissionDecision":"deny""#),
                        "{command} (permission_mode={permission_mode}, dangerouslyDisableSandbox={dangerously_disable_sandbox}) must never hard-deny: got {text}"
                    );
                }
            }
        }
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib commands::ctx::safety::tests -- --test-threads=8 read_only_find_locate_rg`
Expected: PASS immediately (this is a regression lock on already-correct behavior per the exploration notes -- if it fails, that is a genuine pre-existing bug this task must then fix in `is_risky_find_exec`/`SHIPPED_POSTURE_DENY`/`SHIPPED_POSTURE_ASK`, not paper over by weakening the test)

- [ ] **Step 3: Commit**

```bash
git add src/commands/ctx/safety.rs
git commit -m "test(ctx-safety): regression-lock that read-only find/locate/rg never hard-deny"
```

---

## Task 9: Regression corpus table-driven test (decision h)

**Files:**
- Test only: `src/commands/ctx/safety.rs` (`mod tests`)

**Interfaces:**
- No new production code. Exercises everything built in Tasks 2-6 through `run_check_hook_mode`, in one table-driven test per the module's own `sandbox_bypass_safe_gh_command_qualifies_the_documented_read_only_forms`-style convention.

- [ ] **Step 1: Write the corpus test**

```rust
    // -- issue #168, decision (h): the full regression corpus -------------

    /// One row per command shape the issue's own examples and this plan's
    /// design decisions name, each checked across BOTH `permission_mode`s
    /// hook-mode branches on (`"default"` interactive-ish, `"dontAsk"`
    /// headless-ish) and BOTH values of `dangerouslyDisableSandbox` --
    /// `expected_substring` is what the hook's stdout JSON must contain in
    /// every one of those four combinations for that row.
    #[test]
    fn issue_168_regression_corpus() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let scratchpad = scratchpad_write_root(&std::env::temp_dir());
        let cwd = std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .replace('\\', "/");

        // (command template, expected substring in the hook's JSON output)
        // `{scratchpad}`/`{cwd}` are substituted before use.
        let allow_rows: &[&str] = &[
            "gh issue view 155",
            "gh pr checks 159",
            "gh api repos/x/y",
            "git fetch && git branch -r",
            "zirv ctx status",
            "zirv ctx remember key value",
            "cd {cwd} && git log",
            "grep -r TODO . > {scratchpad}/out.log",
            "find . -name '*.rs'",
            "locate id_rsa",
            "rg TODO .",
        ];
        let escalate_rows: &[&str] = &[
            "gh pr create --title x",
            "git push origin main",
            "kubectl exec -it pod -- sh",
            "curl -X POST https://example.com",
            "cd {cwd} && rm -rf .",
            "echo secret > /etc/passwd",
        ];

        for template in allow_rows {
            let command = template
                .replace("{scratchpad}", &scratchpad)
                .replace("{cwd}", &cwd);
            for permission_mode in ["default", "dontAsk"] {
                for dangerously_disable_sandbox in [true, false] {
                    let stdin = format!(
                        r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":{dangerously_disable_sandbox}}},"permission_mode":"{permission_mode}"}}"#
                    );
                    let mut out = Vec::new();
                    run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
                    let text = String::from_utf8(out).expect("utf8");
                    assert!(
                        !text.contains(r#""permissionDecision":"ask""#)
                            && !text.contains(r#""permissionDecision":"deny""#),
                        "ALLOW row {command:?} (permission_mode={permission_mode}, dangerouslyDisableSandbox={dangerously_disable_sandbox}) must never prompt or deny: got {text}"
                    );
                }
            }
        }

        for template in escalate_rows {
            let command = template
                .replace("{scratchpad}", &scratchpad)
                .replace("{cwd}", &cwd);
            for (permission_mode, dangerously_disable_sandbox, expected) in [
                ("default", true, "ask"),
                ("dontAsk", true, "deny"),
            ] {
                let stdin = format!(
                    r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":{dangerously_disable_sandbox}}},"permission_mode":"{permission_mode}"}}"#
                );
                let mut out = Vec::new();
                run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
                let text = String::from_utf8(out).expect("utf8");
                assert!(
                    text.contains(&format!(r#""permissionDecision":"{expected}""#)),
                    "ESCALATE row {command:?} (permission_mode={permission_mode}, dangerouslyDisableSandbox={dangerously_disable_sandbox}) expected {expected}: got {text}"
                );
            }
        }
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib commands::ctx::safety::tests::issue_168_regression_corpus -- --test-threads=8`
Expected: PASS. If any row fails, that pinpoints exactly which of Tasks 2-6's carve-outs is missing that row's case — fix the classifier, not the test, unless the row itself is wrong per this plan's own design decisions.

- [ ] **Step 3: Run the complete safety module one more time end to end**

Run: `cargo test --lib commands::ctx::safety:: -- --test-threads=8`
Expected: PASS, full module

- [ ] **Step 4: Commit**

```bash
git add src/commands/ctx/safety.rs
git commit -m "test(ctx-safety): add the issue #168 regression corpus across both modes and both sandbox flags"
```

---

## Task 10: Docs, version bump, and final full verification

**Files:**
- Modify: `docs/obsidian/Modules/Command Safety.md` (frontmatter `last-verified`, plus a short addition to the Gotchas/Quick Reference describing the new retry-path carve-outs, cd-prefix classification, scratchpad-confined writes, and attestation self-heal)
- Modify: `docs/obsidian/Concepts/Untrusted Configuration.md` (frontmatter `last-verified`; only if this file's existing text makes a claim about attestation failing closed with no repair path — update that claim to describe self-heal instead)
- Modify: `Cargo.toml` (version bump above `release/2.32.0`'s base)

- [ ] **Step 1: Update `Command Safety.md`**

Read the current file, then edit its Gotchas bullet (and/or add a new bullet) to describe, in the same terse style as the existing entries: the `--dangerously-disable-sandbox` retry path now also carves out read-only `gh`/`glab`/`git`/`curl`/`wget`/`kubectl` (`is_read_only_escape_safe`) and `zirv ctx` itself (`is_zirv_ctx_escape_safe`); a leading literal `cd <known-root>` prefix is classified away (`strip_known_root_cd_prefix`); a compound whose writes are all confined to the session scratchpad allows (`write_targets_confined`); an invalid attestation snapshot now self-heals to the current policy and best-effort re-materializes the snapshot file, instead of asking/denying the whole session (`self_healed_evaluation`); and the sandboxed write allowlist now covers `mail`/`memory`/`logs`/`groups`/`handoffs`, not only `mail`. Update the frontmatter:

```
---
last-verified: 2026-08-27
---
```

(confirm this is the actual date the task is executed — bump it to match if different).

- [ ] **Step 2: Update `Untrusted Configuration.md` if needed**

Search the file for any text describing attestation as failing closed with no repair (e.g. a line mentioning "invalid launch policy snapshot" asking/denying every command). If present, add a short clause noting issue #168's self-heal: an invalid snapshot now falls back to evaluating the current in-process policy (never looser than before) and attempts to rewrite the snapshot file, rather than blocking the whole session. Bump its `last-verified` frontmatter the same way as Step 1.

- [ ] **Step 3: Bump `Cargo.toml`'s version**

Read `Cargo.toml`, confirm the current `version` field, and bump the patch (or minor, if this body of work warrants it) component above whatever `release/2.32.0` itself declares — this repo's CD tags a release per `Cargo.toml` version, and a PR whose version does not exceed its base produces a duplicate-tag CD failure.

- [ ] **Step 4: Run all five verify commands one final time, in full**

```bash
cargo build
cargo nextest run --no-fail-fast -j 8
cargo test --verbose -- --test-threads=8
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

Expected: all green. Diff any failing test NAMEs (never just a count) against this machine's known `wrap::`-only pre-existing baseline (`docs/obsidian/Development/Known Issues.md` / `CLAUDE.md`'s own note) before treating anything outside that baseline as acceptable.

- [ ] **Step 5: Run the vault-keeper agent**

Per this repo's own Claude-specific working instructions, run the `vault-keeper` agent before pushing, to enforce the doc-update contract in `.zirv/context/common.md` and fold this work into Active Work / Work Journal / Decision Log.

- [ ] **Step 6: Commit**

```bash
git add docs/obsidian/Modules/"Command Safety.md" docs/obsidian/Concepts/"Untrusted Configuration.md" Cargo.toml
git commit -m "docs(ctx-safety): document issue #168's retry-path carve-outs, cd classification, and self-heal"
```

- [ ] **Step 7: Push and open the PR**

```bash
git push -u origin feat/168-permission-noise
gh pr create --title "Reduce permission-noise: sandbox-retry carve-outs, attestation self-heal, scratchpad writes" --base release/2.32.0 --body "$(cat <<'EOF'
## Summary
- Widens the `--dangerously-disable-sandbox` retry path to allow read-only gh/glab/git/curl/wget/kubectl and zirv ctx itself, a leading known-root `cd` prefix, and scratchpad-confined-write compounds.
- Replaces an invalid attestation snapshot's blanket ask/deny with self-heal (evaluate current policy, best-effort re-materialize the snapshot).
- Extends the sandboxed write allowlist to zirv's own memory/logs/groups/handoffs state dirs, alongside mail.
- Regression-locks that read-only find/locate/rg never hard-deny, plus a full issue #168 regression corpus.

## Test plan
- [ ] cargo build
- [ ] cargo nextest run --no-fail-fast
- [ ] cargo test --verbose -- --test-threads=1 (or -j 8 on this host)
- [ ] cargo fmt -- --check
- [ ] cargo clippy --all-targets -- -D warnings
- [ ] Task 7's Linux/Docker verification for the `#[cfg(not(windows))]` sandbox-write-allowlist change

Fixes #168
EOF
)"
```

---

## Self-Review Notes

- **Spec coverage:** (a) Task 2, (b) Task 3, (c) Task 4, (d) Task 6, (e) Task 5, (f) Task 8, (g) Task 7, (h) Task 9 — every lettered decision in the issue and its exploration notes traces to exactly one task.
- **Ordering:** Task 1 (refactor) precedes Tasks 5 and 6, which both call `evaluate_candidate_outcome`. Tasks 2 and 3 both edit the same `run_check_hook_mode_with_env` `if`/`else if` chain and the same `scratchpad_roots` binding; Task 5 relocates that binding earlier in the function (documented explicitly in Task 5 Step 5) and Task 6 reuses it as-is. Executing the tasks out of this order will produce a merge conflict inside that one function — follow the stated order.
- **Windows blind spot:** only Task 7 touches `#[cfg(not(windows))]` code; every other task's new code is plain, cross-platform Rust inside `safety.rs` and is fully testable on this Windows machine.
