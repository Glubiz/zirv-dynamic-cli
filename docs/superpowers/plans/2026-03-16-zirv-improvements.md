# Zirv CLI Improvements Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Improve zirv's error reporting, UX, and robustness with colored output, placeholder validation, dry-run mode, proper exit codes, step progress, and script validation.

**Architecture:** Add a thin `output` module wrapping `console` crate for colored stderr/stdout. Extend `Input` with `--dry-run` flag. Add validation in `build_context` and `Script::run`. Replace deprecated `serde_yaml` with `serde_yml`. Propagate real exit codes via `std::process::exit`.

**Tech Stack:** Rust, console crate (already transitive dep via dialoguer), serde_yml, clap, regex (new dep for placeholder detection)

---

## Chunk 1: Foundation (Tasks 1-3)

### Task 1: Replace `serde_yaml` with `serde_yml`

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/utils.rs`
- Modify: `src/input.rs`
- Modify: `src/commands/help.rs`
- Modify: `src/commands/create.rs`

- [ ] **Step 1: Update Cargo.toml**

Replace `serde_yaml = "0.9.34+deprecated"` with `serde_yml = "0.0.12"` in `[dependencies]`.

- [ ] **Step 2: Replace all `serde_yaml` references with `serde_yml`**

In every file that uses `serde_yaml::from_str` or `serde_yaml::to_string`, replace with `serde_yml::from_str` / `serde_yml::to_string`.

Files to update:
- `src/utils.rs:28` — `serde_yaml::from_str(content)?` → `serde_yml::from_str(content)?`
- `src/input.rs:30` — `serde_yaml::from_str(&content)?` → `serde_yml::from_str(&content)?`
- `src/commands/help.rs:43` — `serde_yaml::from_str(&content)?` → `serde_yml::from_str(&content)?`
- `src/commands/create.rs:75` — `serde_yaml::from_str(&content).unwrap_or_default()` → `serde_yml::from_str(&content).unwrap_or_default()`
- `src/commands/create.rs:82` — `serde_yaml::to_string(&shortcuts)?` → `serde_yml::to_string(&shortcuts)?`

- [ ] **Step 3: Run `cargo update` to refresh lockfile**

Run: `cargo update`

- [ ] **Step 4: Build and test**

Run: `cargo build && cargo test --verbose -- --test-threads=1`
Expected: All 18 tests pass, no `serde_yaml` deprecation warning.

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: Clean

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/utils.rs src/input.rs src/commands/help.rs src/commands/create.rs
git commit -m "Replace deprecated serde_yaml with serde_yml"
```

---

### Task 2: Add `console` as direct dependency and create output module

**Files:**
- Modify: `Cargo.toml`
- Create: `src/output.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add `console` to Cargo.toml**

Add `console = "0.16.3"` under `[dependencies]`. It's already a transitive dep via dialoguer, so this just makes it a direct one.

- [ ] **Step 2: Create `src/output.rs`**

```rust
use console::style;
use std::fmt::Display;

pub fn error(msg: impl Display) {
    eprintln!("{} {msg}", style("error:").red().bold());
}

pub fn warn(msg: impl Display) {
    eprintln!("{} {msg}", style("warning:").yellow().bold());
}

pub fn step(index: usize, total: usize, cmd: &str) {
    eprintln!(
        "{} {}",
        style(format!("[{}/{}]", index + 1, total)).dim().bold(),
        cmd
    );
}

pub fn step_description(description: &str) {
    eprintln!("  {}", style(description).dim());
}

pub fn dry_run(index: usize, total: usize, cmd: &str) {
    eprintln!(
        "{} {} {}",
        style(format!("[{}/{}]", index + 1, total)).dim().bold(),
        style("[dry-run]").cyan().bold(),
        cmd
    );
}

pub fn skipped(reason: &str) {
    eprintln!("  {}", style(format!("skipped: {reason}")).dim());
}
```

- [ ] **Step 3: Register module in main.rs**

Add `mod output;` to `src/main.rs` after `mod utils;`.

- [ ] **Step 4: Build**

Run: `cargo build`
Expected: Compiles with no errors.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/output.rs src/main.rs
git commit -m "Add output module for colored CLI output"
```

---

### Task 3: Use colored output throughout the codebase

**Files:**
- Modify: `src/main.rs`
- Modify: `src/script_runner/command.rs`
- Modify: `src/script_runner/fallback_command.rs`
- Modify: `src/script_runner/script.rs`
- Modify: `src/script_runner/mod.rs`

- [ ] **Step 1: Update `main.rs` error handling**

Replace the current error output in `main.rs:46-52`:

```rust
// Before:
match execute(&script, &input.params).await {
    Ok(_) => Ok(()),
    Err(e) => {
        eprintln!("{e}");
        Err(e.into())
    }
}

// After:
match execute(&script, &input.params).await {
    Ok(_) => Ok(()),
    Err(e) => {
        output::error(&e);
        Err(e.into())
    }
}
```

- [ ] **Step 2: Update `command.rs` — replace println with output functions**

In `Command::invoke` (`src/script_runner/command.rs:113-116`), replace:
```rust
println!("Executing command: {command}");
if let Some(description) = &self.description {
    println!("Description: {description}");
}
```
With:
```rust
// Remove these lines — step output is now handled by Script::run
```

The step/description printing moves to `Script::run` so it can include the step counter. The `invoke` method should be silent.

- [ ] **Step 3: Update `fallback_command.rs` — replace println with output functions**

In `FallbackCommand::invoke` (`src/script_runner/fallback_command.rs:28-31`), replace:
```rust
println!("Executing command: {}", &self.command);
if let Some(description) = &self.description {
    println!("Description: {description}");
}
```
With:
```rust
crate::output::warn(format!("fallback: {}", &self.command));
if let Some(description) = &self.description {
    crate::output::step_description(description);
}
```

- [ ] **Step 4: Update `script.rs` — add step progress indicator**

Modify `Script::run` to pass step index and total to the output module. Change the method signature to accept a `dry_run: bool` parameter (will be used in Task 6).

```rust
pub async fn run(&self, context: &mut HashMap<String, String>, dry_run: bool) -> Result<(), String> {
    let total = self.commands.len();
    for (index, step) in self.commands.iter().enumerate() {
        let cmd_display = step.display(context);
        if dry_run {
            crate::output::dry_run(index, total, &cmd_display);
            continue;
        }
        crate::output::step(index, total, &cmd_display);
        if let Some(desc) = step.description() {
            crate::output::step_description(&desc);
        }
        match step.execute(context).await {
            Ok(Some(output)) => {
                crate::output::skipped(&output);
            }
            Ok(None) => {}
            Err(e) => {
                crate::output::error(format!(
                    "step {}/{} in script '{}': {}",
                    index + 1,
                    total,
                    self.name,
                    e
                ));
                return Err(e);
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Add `display` and `description` methods to `CommandTypes`**

In `src/script_runner/command_types.rs`, add:

```rust
impl CommandTypes {
    pub fn display(&self, context: &HashMap<String, String>) -> String {
        match self {
            CommandTypes::Command(cmd) => cmd.substituted_command(context),
            CommandTypes::Commands(cmds) => {
                let joined = cmds.iter().map(|c| c.command.as_str()).collect::<Vec<_>>().join(" && ");
                format!("[multi-shell] {joined}")
            }
        }
    }

    pub fn description(&self) -> Option<String> {
        match self {
            CommandTypes::Command(cmd) => cmd.description.clone(),
            CommandTypes::Commands(_) => None,
        }
    }
    // ... existing execute method stays
}
```

Make `Command::substituted_command` `pub` (it's currently `fn`, not `pub fn`).

- [ ] **Step 6: Update `script_runner/mod.rs` — pass `dry_run: false` for now**

Update the `execute` function signature and call:

```rust
pub async fn execute(script: &Script, params: &[String], dry_run: bool) -> Result<(), String> {
    let mut context = build_context(script, params)?;
    script.run(&mut context, dry_run).await?;
    Ok(())
}
```

Update `main.rs` call to pass `false`:
```rust
match execute(&script, &input.params, false).await {
```

- [ ] **Step 7: Build and test**

Run: `cargo build && cargo test --verbose -- --test-threads=1`
Expected: All tests pass (update test calls to `script.run(&mut context, false)` where needed).

- [ ] **Step 8: Run clippy and fmt**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: Clean

- [ ] **Step 9: Commit**

```bash
git add src/
git commit -m "Add colored output and step progress indicators"
```

---

## Chunk 2: Validation and Safety (Tasks 4-5)

### Task 4: Unresolved `${placeholder}` detection

**Files:**
- Modify: `Cargo.toml` (add `regex`)
- Modify: `src/script_runner/command.rs`
- Modify: `src/script_runner/command_types.rs`

- [ ] **Step 1: Add `regex` dependency**

Add to Cargo.toml `[dependencies]`: `regex = "1"`

- [ ] **Step 2: Write test for unresolved placeholder detection**

Add to `src/script_runner/command.rs` tests:

```rust
#[tokio::test]
async fn test_unresolved_placeholder_detected() {
    let command = Command {
        command: "echo ${name} ${typo}".to_string(),
        capture: None,
        description: None,
        options: None,
    };

    let mut context = HashMap::new();
    context.insert("name".to_string(), "Alice".to_string());

    let result = command.check_unresolved_placeholders(&context);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("typo"));
}

#[tokio::test]
async fn test_no_unresolved_placeholders() {
    let command = Command {
        command: "echo ${name}".to_string(),
        capture: None,
        description: None,
        options: None,
    };

    let mut context = HashMap::new();
    context.insert("name".to_string(), "Alice".to_string());

    let result = command.check_unresolved_placeholders(&context);
    assert!(result.is_ok());
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test command::tests::test_unresolved -- --test-threads=1`
Expected: FAIL — method does not exist yet.

- [ ] **Step 4: Implement `check_unresolved_placeholders`**

Add to `Command` impl in `src/script_runner/command.rs`:

```rust
pub fn check_unresolved_placeholders(
    &self,
    context: &HashMap<String, String>,
) -> Result<(), String> {
    let substituted = self.substituted_command(context);
    let re = regex::Regex::new(r"\$\{([^}]+)\}").unwrap();
    let unresolved: Vec<&str> = re
        .captures_iter(&substituted)
        .map(|c| c.get(1).unwrap().as_str())
        .collect();
    if unresolved.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Unresolved placeholders in '{}': {}",
        self.command,
        unresolved.join(", ")
    ))
}
```

- [ ] **Step 5: Call check from `Command::execute` before invoking**

In `Command::execute`, after `let command = self.substituted_command(context);` and before the `cd` check, add:

```rust
self.check_unresolved_placeholders(context)?;
```

- [ ] **Step 6: Also check in `CommandTypes::Commands` variant**

In `CommandTypes::execute` for the `Commands` variant, after substitution and before joining, check each command:

```rust
for cmd in &substituted {
    let re = regex::Regex::new(r"\$\{([^}]+)\}").unwrap();
    let unresolved: Vec<&str> = re
        .captures_iter(&cmd.command)
        .map(|c| c.get(1).unwrap().as_str())
        .collect();
    if !unresolved.is_empty() {
        return Err(format!(
            "Unresolved placeholders in '{}': {}",
            cmd.command,
            unresolved.join(", ")
        ));
    }
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test --verbose -- --test-threads=1`
Expected: All tests pass including new ones.

- [ ] **Step 8: Run clippy and fmt**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock src/script_runner/command.rs src/script_runner/command_types.rs
git commit -m "Detect unresolved placeholders before execution"
```

---

### Task 5: Script validation on load

**Files:**
- Modify: `src/script_runner/script.rs`
- Modify: `src/script_runner/mod.rs`

- [ ] **Step 1: Write test for optional-before-required validation**

Add to `src/script_runner/mod.rs` tests:

```rust
#[tokio::test]
async fn test_optional_before_required_rejected() {
    let script = make_script(vec!["optional?".to_string(), "required".to_string()]);
    let result = build_context(&script, &["a".to_string(), "b".to_string()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Optional parameters must come after all required parameters"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_optional_before_required_rejected -- --test-threads=1`
Expected: FAIL — currently accepts this ordering.

- [ ] **Step 3: Add validation to `build_context`**

In `build_context`, after computing `required_count` and `total_count`, add:

```rust
let mut seen_optional = false;
for name in names {
    let is_optional = name.ends_with('?');
    if seen_optional && !is_optional {
        return Err(
            "Optional parameters must come after all required parameters".to_string(),
        );
    }
    seen_optional = is_optional;
}
```

- [ ] **Step 4: Write test for duplicate param names**

```rust
#[tokio::test]
async fn test_duplicate_param_names_rejected() {
    let script = make_script(vec!["name".to_string(), "name".to_string()]);
    let result = build_context(&script, &["a".to_string(), "b".to_string()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Duplicate parameter name"));
}

#[tokio::test]
async fn test_duplicate_optional_param_names_rejected() {
    let script = make_script(vec!["name".to_string(), "name?".to_string()]);
    let result = build_context(&script, &["a".to_string(), "b".to_string()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Duplicate parameter name"));
}
```

- [ ] **Step 5: Add duplicate name validation**

After the ordering check, add:

```rust
let mut seen_names = std::collections::HashSet::new();
for name in names {
    let clean = name.strip_suffix('?').unwrap_or(name);
    if !seen_names.insert(clean) {
        return Err(format!("Duplicate parameter name: '{clean}'"));
    }
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test --verbose -- --test-threads=1`
Expected: All tests pass.

- [ ] **Step 7: Run clippy and fmt**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`

- [ ] **Step 8: Commit**

```bash
git add src/script_runner/mod.rs
git commit -m "Validate param ordering and detect duplicates"
```

---

## Chunk 3: CLI Features (Tasks 6-7)

### Task 6: Dry-run mode

**Files:**
- Modify: `src/input.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add `--dry-run` flag to Input**

In `src/input.rs`, add to the `Input` struct:

```rust
#[arg(long, default_value_t = false)]
pub dry_run: bool,
```

- [ ] **Step 2: Pass dry_run through in main.rs**

Update the `execute` call in `main.rs`:

```rust
match execute(&script, &input.params, input.dry_run).await {
```

- [ ] **Step 3: Build and test**

Run: `cargo build && cargo test --verbose -- --test-threads=1`
Expected: All tests pass.

- [ ] **Step 4: Manual test**

Run: `cargo run -- commit --dry-run` (or any existing script in .zirv/)
Expected: Shows `[1/N] [dry-run] <command>` for each step, doesn't execute.

- [ ] **Step 5: Commit**

```bash
git add src/input.rs src/main.rs
git commit -m "Add --dry-run flag to preview commands without executing"
```

---

### Task 7: Proper exit codes

**Files:**
- Modify: `src/main.rs`
- Modify: `src/script_runner/command.rs`

- [ ] **Step 1: Propagate exit codes from failed commands**

In `Command::invoke`, change the error returns to include the exit code. Update the error format in the `else` branch (non-capture, `src/script_runner/command.rs:139-143`):

```rust
// Before:
if !status.success() {
    return Err(format!("`{command}` failed").into());
}

// After:
if !status.success() {
    let code = status.code().unwrap_or(1);
    return Err(format!("`{command}` failed with exit code {code}").into());
}
```

Do the same for the capture branch (`src/script_runner/command.rs:129-131`):

```rust
if !out.status.success() {
    let code = out.status.code().unwrap_or(1);
    return Err(format!("`{command}` failed with exit code {code}").into());
}
```

- [ ] **Step 2: Use `std::process::exit` in main.rs**

Replace the error handling in main to use explicit exit codes instead of returning `Err` (which causes tokio to print a redundant `Error: ...`):

```rust
match execute(&script, &input.params, input.dry_run).await {
    Ok(_) => {}
    Err(e) => {
        output::error(&e);
        std::process::exit(1);
    }
}
```

Change `main` return type from `Result<(), Box<dyn std::error::Error>>` to `()`. Update all the early `return Ok(())` to just `return` and remove `Ok(())` at end of match arms. Handle errors from built-in commands similarly:

```rust
#[tokio::main]
async fn main() {
    let input = Input::parse();

    match input.command.as_str() {
        "help" | "h" => {
            if let Err(e) = show_help(&mut std::io::stdout()) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "version" | "v" => {
            if let Err(e) = get_version(&mut std::io::stdout()) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "init" | "i" => {
            if let Err(e) = init_zirv() {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "create" | "c" => {
            if let Err(e) = create_script_interactive() {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        _ => {}
    }

    let file_path = match input.get_file_path() {
        Ok(p) => p,
        Err(e) => {
            output::error(e);
            std::process::exit(1);
        }
    };

    let script = match file_to_script(&file_path) {
        Ok(s) => s,
        Err(e) => {
            output::error(e);
            std::process::exit(1);
        }
    };

    if let Err(e) = execute(&script, &input.params, input.dry_run).await {
        output::error(&e);
        std::process::exit(1);
    }
}
```

- [ ] **Step 3: Build and test**

Run: `cargo build && cargo test --verbose -- --test-threads=1`
Expected: All tests pass.

- [ ] **Step 4: Run clippy and fmt**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/script_runner/command.rs
git commit -m "Improve exit codes and remove redundant error output"
```

---

## Final Steps

- [ ] **Run full CI check**

```bash
cargo test --verbose -- --test-threads=1
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

- [ ] **Final commit if any formatting changes needed**

- [ ] **Push and update PR**
