//! Issue #418: native `PreToolUse`/`PostToolUse`-equivalent hook seams for
//! agents whose own CLI supports a user-level hooks configuration file,
//! distinct from claude's `~/.claude/settings.json` wiring in `setup.rs`
//! (`HARNESS_HOOKS`/`ensure_harness_hook`). This module is the generic,
//! payload-agnostic half: pure operations over `serde_json::Value` plus
//! thin, atomic-write filesystem wrappers. Each adapter's own `native_hooks`
//! (`AgentAdapter::native_hooks`, default `None`) supplies the per-agent file
//! path and hook-entry shapes; `hook_project.rs` supplies the payload
//! projection those installed commands need at runtime.
//!
//! Every fact this module's own file/shape choices rest on is DOCS-ONLY,
//! researched 2026-09-09 against each vendor's published reference docs, not
//! a live binary -- see `copilot.rs`'s, `droid.rs`'s and `gemini.rs`'s own
//! `native_hooks` doc comments for the citations, following the "verified
//! facts and their source" convention their module doc comments already use.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::CtxResult;
use super::state;

/// A prefix every command string this module installs starts with. Also the
/// ownership marker: an array element is "zirv-owned" when ANY string value
/// nested inside it starts with this prefix, mirroring `setup::
/// contains_command`'s own "somewhere in the JSON tree" search but scoped to
/// one element rather than the whole file, since a native hooks file (droid,
/// gemini) may hold an operator's own unrelated entries alongside zirv's.
const ZIRV_COMMAND_PREFIX: &str = "zirv ctx ";

/// One hook entry an installer ensures exists inside the array named by
/// `pointer` -- a JSON Pointer's path segments to the array that holds
/// `element` (not the element itself), created along the way if any
/// intermediate object is missing. Several entries may share one `pointer`
/// (e.g. copilot's own guard and safety-check hooks both live in
/// `hooks.preToolUse`): they are told apart by the zirv-owned command
/// string(s) `element` itself carries, never by "is any zirv element already
/// in this array at all".
#[derive(Debug, Clone)]
pub struct NativeHookEntry {
    pub pointer: Vec<String>,
    pub element: Value,
    pub label: &'static str,
}

/// One agent's whole native-hooks install target: the file, whether zirv owns
/// the file outright (`owned_file: true` -- uninstall deletes it; never true
/// for a file an operator's own hooks might also live in), and the entries to
/// ensure/remove inside it.
#[derive(Debug, Clone)]
pub struct NativeHooks {
    pub file: PathBuf,
    pub owned_file: bool,
    pub entries: Vec<NativeHookEntry>,
    /// Fields merged into a brand-new (currently-`{}`) root before any entry
    /// is applied -- e.g. copilot's own `"version":1`. Never overwrites a key
    /// that is already present (an existing file, zirv-owned or not, is
    /// never touched here), so this only ever seeds a file this call itself
    /// creates.
    pub root_defaults: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    Installed,
    Missing,
}

#[derive(Debug, Clone, Default)]
pub struct InstallReport {
    /// Labels of the entries this call actually inserted. Empty on a second,
    /// idempotent run against an already-installed file.
    pub written: Vec<&'static str>,
}

#[derive(Debug, Clone, Default)]
pub struct UninstallReport {
    /// Whether `NativeHooks::file` itself was deleted (only ever true when
    /// `owned_file` is set).
    pub file_removed: bool,
    /// Labels of the entries actually removed from a shared file (never
    /// populated when `owned_file` is set -- the whole file went instead).
    pub removed: Vec<&'static str>,
}

/// Whether any string value reachable from `value` starts with
/// [`ZIRV_COMMAND_PREFIX`] -- the ownership test both `install` (per-entry
/// "already present") and `uninstall` (per-element "safe to remove") apply.
fn value_is_zirv_owned(value: &Value) -> bool {
    match value {
        Value::String(s) => s.starts_with(ZIRV_COMMAND_PREFIX),
        Value::Object(map) => map.values().any(value_is_zirv_owned),
        Value::Array(items) => items.iter().any(value_is_zirv_owned),
        _ => false,
    }
}

/// Every zirv-owned command string reachable from `value`, in document order.
/// Used to tell two zirv-owned entries in the same array apart by WHICH
/// command they carry (e.g. `hook pretool` vs `safety check`), rather than
/// merely whether either is present at all.
fn zirv_command_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.starts_with(ZIRV_COMMAND_PREFIX) => out.push(s.clone()),
        Value::Object(map) => {
            for v in map.values() {
                zirv_command_strings(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                zirv_command_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Read-only lookup of the array living at `pointer`'s path inside `root`.
/// `None` when any segment is missing or the terminal value is not an array
/// -- never an error: a not-yet-installed file simply has nothing there.
fn array_at<'a>(root: &'a Value, pointer: &[String]) -> Option<&'a Vec<Value>> {
    let mut current = root;
    for seg in pointer {
        current = current.get(seg)?;
    }
    current.as_array()
}

/// Ensures every object along `pointer` exists (creating `{}` as needed,
/// and coercing a non-object/non-array in the way to the shape this path
/// needs), then ensures the terminal segment holds an array, and returns a
/// mutable handle to it. `None` only for an empty `pointer` -- every
/// `NativeHookEntry` this module ships always has at least one segment.
fn ensure_array_at<'a>(root: &'a mut Value, pointer: &[String]) -> Option<&'a mut Vec<Value>> {
    let (last, init) = pointer.split_last()?;
    let mut current = root;
    for seg in init {
        if !current.is_object() {
            *current = json!({});
        }
        current = current
            .as_object_mut()
            .expect("just coerced to an object")
            .entry(seg.clone())
            .or_insert_with(|| json!({}));
    }
    if !current.is_object() {
        *current = json!({});
    }
    let entry = current
        .as_object_mut()
        .expect("just coerced to an object")
        .entry(last.clone())
        .or_insert_with(|| json!([]));
    if !entry.is_array() {
        *entry = json!([]);
    }
    entry.as_array_mut()
}

/// Loads `path` as a JSON object, `{}` when it does not exist yet. Errors on
/// a file that exists but is not valid JSON or not an object -- the same
/// refusal `setup::load_json_object` already makes for claude's own
/// `settings.json`, so a malformed file is never silently overwritten.
fn load(path: &Path) -> CtxResult<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if !value.is_object() {
        return Err(format!("{} must contain a JSON object", path.display()).into());
    }
    Ok(value)
}

fn write(path: &Path, value: &Value) -> CtxResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value)? + "\n";
    state::write_atomic(path, &text, false)?;
    Ok(())
}

/// Ensures every entry of `hooks` is present, appending only what is
/// missing. Idempotent: a second call against an already-installed file
/// reports an empty `written` and leaves the file byte-identical (no write
/// happens at all when nothing changed).
pub fn install(hooks: &NativeHooks) -> CtxResult<InstallReport> {
    let mut root = load(&hooks.file)?;
    if let Value::Object(defaults) = &hooks.root_defaults
        && let Value::Object(existing) = &mut root
    {
        for (key, value) in defaults {
            existing.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    let mut written = Vec::new();
    for entry in &hooks.entries {
        let mut commands = Vec::new();
        zirv_command_strings(&entry.element, &mut commands);
        let already_present = array_at(&root, &entry.pointer).is_some_and(|arr| {
            arr.iter().any(|existing| {
                let mut existing_commands = Vec::new();
                zirv_command_strings(existing, &mut existing_commands);
                existing_commands
                    .iter()
                    .any(|command| commands.contains(command))
            })
        });
        if already_present {
            continue;
        }
        if let Some(arr) = ensure_array_at(&mut root, &entry.pointer) {
            arr.push(entry.element.clone());
            written.push(entry.label);
        }
    }
    if !written.is_empty() {
        write(&hooks.file, &root)?;
    }
    Ok(InstallReport { written })
}

/// Removes only zirv-owned elements. `owned_file` deletes the whole file
/// instead (and never touches `entries` at all); otherwise each entry's own
/// array is filtered in place, leaving every non-zirv-owned element (an
/// operator's own hook) untouched, and the file is rewritten only when
/// something actually changed.
pub fn uninstall(hooks: &NativeHooks) -> CtxResult<UninstallReport> {
    if hooks.owned_file {
        let file_removed = hooks.file.exists();
        if file_removed {
            std::fs::remove_file(&hooks.file)?;
        }
        return Ok(UninstallReport {
            file_removed,
            removed: Vec::new(),
        });
    }
    let mut root = load(&hooks.file)?;
    let mut removed = Vec::new();
    for entry in &hooks.entries {
        if let Some(arr) = ensure_array_at(&mut root, &entry.pointer) {
            let before = arr.len();
            arr.retain(|v| !value_is_zirv_owned(v));
            if arr.len() != before {
                removed.push(entry.label);
            }
        }
    }
    if !removed.is_empty() {
        write(&hooks.file, &root)?;
    }
    Ok(UninstallReport {
        file_removed: false,
        removed,
    })
}

/// One `(label, state)` row per entry `hooks` declares, read-only: never
/// writes, never creates the file.
pub fn status(hooks: &NativeHooks) -> CtxResult<Vec<(&'static str, InstallState)>> {
    let root = load(&hooks.file)?;
    Ok(hooks
        .entries
        .iter()
        .map(|entry| {
            let mut commands = Vec::new();
            zirv_command_strings(&entry.element, &mut commands);
            let installed = array_at(&root, &entry.pointer).is_some_and(|arr| {
                arr.iter().any(|existing| {
                    let mut existing_commands = Vec::new();
                    zirv_command_strings(existing, &mut existing_commands);
                    existing_commands
                        .iter()
                        .any(|command| commands.contains(command))
                })
            });
            (
                entry.label,
                if installed {
                    InstallState::Installed
                } else {
                    InstallState::Missing
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copilot_hooks(dir: &Path) -> NativeHooks {
        NativeHooks {
            file: dir.join(".copilot").join("hooks").join("zirv.json"),
            owned_file: true,
            root_defaults: json!({"version": 1}),
            entries: vec![
                NativeHookEntry {
                    pointer: vec!["hooks".to_string(), "preToolUse".to_string()],
                    element: json!({
                        "type": "command",
                        "bash": "zirv ctx hook pretool --agent copilot",
                        "powershell": "zirv ctx hook pretool --agent copilot",
                        "timeoutSec": 30
                    }),
                    label: "pretool guard",
                },
                NativeHookEntry {
                    pointer: vec!["hooks".to_string(), "preToolUse".to_string()],
                    element: json!({
                        "matcher": "bash",
                        "type": "command",
                        "bash": "zirv ctx safety check --agent copilot",
                        "powershell": "zirv ctx safety check --agent copilot",
                        "timeoutSec": 30
                    }),
                    label: "safety check",
                },
                NativeHookEntry {
                    pointer: vec!["hooks".to_string(), "postToolUse".to_string()],
                    element: json!({
                        "matcher": "bash",
                        "type": "command",
                        "bash": "zirv ctx hook posttool --agent copilot",
                        "powershell": "zirv ctx hook posttool --agent copilot",
                        "timeoutSec": 30
                    }),
                    label: "posttool compaction",
                },
            ],
        }
    }

    fn droid_hooks(dir: &Path) -> NativeHooks {
        NativeHooks {
            file: dir.join(".factory").join("hooks.json"),
            owned_file: false,
            root_defaults: json!({}),
            entries: vec![
                NativeHookEntry {
                    pointer: vec!["PreToolUse".to_string()],
                    element: json!({
                        "matcher": "Execute|Edit|Create|ApplyPatch",
                        "hooks": [{"type": "command", "command": "zirv ctx hook pretool --agent droid", "timeout": 30}]
                    }),
                    label: "pretool guard",
                },
                NativeHookEntry {
                    pointer: vec!["PreToolUse".to_string()],
                    element: json!({
                        "matcher": "Execute",
                        "hooks": [{"type": "command", "command": "zirv ctx safety check --agent droid", "timeout": 30}]
                    }),
                    label: "safety check",
                },
            ],
        }
    }

    #[test]
    fn install_writes_each_entry_once_and_a_second_run_is_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = copilot_hooks(dir.path());

        let first = install(&hooks).expect("install");
        assert_eq!(
            first.written,
            vec!["pretool guard", "safety check", "posttool compaction"]
        );
        let after_first = std::fs::read_to_string(&hooks.file).expect("read");

        let second = install(&hooks).expect("install");
        assert!(second.written.is_empty(), "idempotent: nothing left to add");
        let after_second = std::fs::read_to_string(&hooks.file).expect("read");
        assert_eq!(
            after_first, after_second,
            "second run must not touch the file"
        );
    }

    #[test]
    fn install_seeds_root_defaults_only_on_a_fresh_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = copilot_hooks(dir.path());
        install(&hooks).expect("install");
        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks.file).expect("read"))
                .expect("json");
        assert_eq!(root.get("version").and_then(Value::as_u64), Some(1));
    }

    #[test]
    fn uninstall_on_a_shared_file_removes_only_the_zirv_owned_element() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = droid_hooks(dir.path());
        std::fs::create_dir_all(hooks.file.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks.file,
            serde_json::to_string(&json!({
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "operators-own-hook"}]}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        install(&hooks).expect("install");
        let report = uninstall(&hooks).expect("uninstall");
        assert!(!report.file_removed);
        // Both zirv entries share one array pointer, so the very first
        // `retain` already clears both of them at once; the second entry's
        // own pass over the now-clean array finds nothing left to remove.
        assert!(!report.removed.is_empty());

        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks.file).expect("read"))
                .expect("json");
        let array = root.get("PreToolUse").and_then(Value::as_array).unwrap();
        assert_eq!(array.len(), 1, "the operator's own entry survives");
        assert!(!value_is_zirv_owned(&array[0]));
    }

    #[test]
    fn uninstall_on_copilot_deletes_only_the_zirv_owned_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = copilot_hooks(dir.path());
        install(&hooks).expect("install");
        // A sibling file (another operator hook) must survive.
        let sibling = hooks.file.parent().unwrap().join("operator.json");
        std::fs::write(&sibling, "{}").unwrap();

        let report = uninstall(&hooks).expect("uninstall");
        assert!(report.file_removed);
        assert!(!hooks.file.exists());
        assert!(sibling.exists(), "uninstall must never touch other files");
    }

    #[test]
    fn status_reports_missing_then_installed_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = droid_hooks(dir.path());

        let before = status(&hooks).expect("status");
        assert!(
            before
                .iter()
                .all(|(_, state)| *state == InstallState::Missing)
        );
        assert!(!hooks.file.exists(), "--show must never create the file");

        install(&hooks).expect("install");
        let after = status(&hooks).expect("status");
        assert!(
            after
                .iter()
                .all(|(_, state)| *state == InstallState::Installed)
        );
    }

    #[test]
    fn install_with_two_entries_sharing_one_array_pointer_inserts_both() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = copilot_hooks(dir.path());
        install(&hooks).expect("install");
        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks.file).expect("read"))
                .expect("json");
        let pre = root
            .pointer("/hooks/preToolUse")
            .and_then(Value::as_array)
            .expect("array");
        assert_eq!(
            pre.len(),
            2,
            "guard and safety-check entries both land in preToolUse"
        );
    }
}
