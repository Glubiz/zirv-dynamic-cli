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

/// Mutable counterpart to [`array_at`], with exactly the same contract:
/// `None` when any segment is missing or the terminal value is not an array.
/// Never creates or coerces anything -- unlike [`ensure_array_at`], this is
/// what `uninstall` uses, since removing zirv's own entries from an array
/// that is not there yet (or is not an array at all) has nothing to do.
fn array_at_mut<'a>(root: &'a mut Value, pointer: &[String]) -> Option<&'a mut Vec<Value>> {
    let mut current = root;
    for seg in pointer {
        current = current.get_mut(seg)?;
    }
    current.as_array_mut()
}

/// A short, human-readable name for `value`'s JSON type, used only inside
/// [`ensure_array_at`]'s own type-mismatch error text.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Ensures every object along `pointer` exists -- creating `{}` for a
/// segment that is genuinely MISSING, and nothing else -- then ensures the
/// terminal segment holds an array (creating `[]` when it too is missing),
/// and returns a mutable handle to it.
///
/// **Never coerces an existing value of the wrong shape.** An intermediate
/// segment that already holds something other than an object, or a terminal
/// segment that already holds something other than an array, is refused
/// with an `Err` naming `file` and the pointer path -- nothing is written in
/// that case, since every caller (`install`) only calls `write` after this
/// succeeds. This is the whole reason `install` can never silently replace
/// an operator's own non-array `hooks.PreToolUse` (say) with an empty array
/// and start appending to it.
fn ensure_array_at<'a>(
    root: &'a mut Value,
    file: &Path,
    pointer: &[String],
) -> CtxResult<&'a mut Vec<Value>> {
    let Some((last, init)) = pointer.split_last() else {
        return Err(format!("{}: empty pointer; not modified", file.display()).into());
    };
    let mut current = root;
    let mut walked: Vec<&str> = Vec::new();
    for seg in init {
        if !current.is_object() {
            return Err(format!(
                "{}: {} holds {}, expected an object; not modified",
                file.display(),
                walked.join("/"),
                json_type_name(current)
            )
            .into());
        }
        current = current
            .as_object_mut()
            .expect("just checked is_object")
            .entry(seg.clone())
            .or_insert_with(|| json!({}));
        walked.push(seg.as_str());
    }
    if !current.is_object() {
        return Err(format!(
            "{}: {} holds {}, expected an object; not modified",
            file.display(),
            walked.join("/"),
            json_type_name(current)
        )
        .into());
    }
    let map = current.as_object_mut().expect("just checked is_object");
    match map.get(last.as_str()) {
        None => {
            map.insert(last.clone(), json!([]));
        }
        Some(existing) if !existing.is_array() => {
            walked.push(last.as_str());
            return Err(format!(
                "{}: {} holds {}, expected an array; not modified",
                file.display(),
                walked.join("/"),
                json_type_name(existing)
            )
            .into());
        }
        Some(_) => {}
    }
    Ok(map
        .get_mut(last.as_str())
        .and_then(Value::as_array_mut)
        .expect("just ensured this key holds an array"))
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

/// This crate's `serde_json` has no `preserve_order` feature, so writing a
/// shared file (droid's `hooks.json`, gemini's `settings.json`) through
/// `serde_json::Value` re-serializes EVERY key in the file in sorted order,
/// not the order an operator's own editor left them in -- the same cosmetic
/// reordering `setup.rs`'s own `heal_target` doc comment calls out for why
/// IT patches raw text instead. Accepted here: unlike `heal_target`, this
/// module's own writes are rare (install/uninstall, not a hot self-heal
/// path) and always touch the file's actual content, so a text-preserving
/// patch would not avoid a diff anyway.
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
        let arr = ensure_array_at(&mut root, &hooks.file, &entry.pointer)?;
        arr.push(entry.element.clone());
        written.push(entry.label);
    }
    if !written.is_empty() {
        write(&hooks.file, &root)?;
    }
    Ok(InstallReport { written })
}

/// Whether every element of every array reachable from `value` (at any
/// nesting depth, not only the paths `NativeHooks::entries` names) is
/// zirv-owned. `uninstall`'s `owned_file` branch uses this to decide whether
/// deleting the whole file is actually safe: an operator (or a future zirv
/// version) may have added an array element this walk finds that
/// `entries`'s own pointers never look at, and deleting the file would
/// destroy it right along with zirv's own entries.
fn every_array_element_is_zirv_owned(value: &Value) -> bool {
    match value {
        Value::Array(items) => {
            items.iter().all(value_is_zirv_owned)
                && items.iter().all(every_array_element_is_zirv_owned)
        }
        Value::Object(map) => map.values().all(every_array_element_is_zirv_owned),
        _ => true,
    }
}

/// Removes only zirv-owned elements, using [`array_at_mut`] -- no creation,
/// no coercion, so a shared file's own array that is missing, or that some
/// other tool has since replaced with a non-array value, is simply skipped
/// rather than fabricated or overwritten.
///
/// `owned_file` (copilot's `zirv.json`) is more than a blind delete: after
/// filtering every entry's own array, the WHOLE root is walked
/// ([`every_array_element_is_zirv_owned`]) for any array element that is not
/// zirv's -- an operator (or a future zirv version) may have added content
/// this module's own `entries` pointers do not name. Only when nothing
/// non-zirv-owned survives anywhere is the file actually deleted; otherwise
/// the filtered root is written back, keeping whatever was found.
pub fn uninstall(hooks: &NativeHooks) -> CtxResult<UninstallReport> {
    if hooks.owned_file {
        if !hooks.file.exists() {
            return Ok(UninstallReport::default());
        }
        let mut root = load(&hooks.file)?;
        let mut removed = Vec::new();
        for entry in &hooks.entries {
            if let Some(arr) = array_at_mut(&mut root, &entry.pointer) {
                let before = arr.len();
                arr.retain(|v| !value_is_zirv_owned(v));
                if arr.len() != before {
                    removed.push(entry.label);
                }
            }
        }
        if every_array_element_is_zirv_owned(&root) {
            std::fs::remove_file(&hooks.file)?;
            return Ok(UninstallReport {
                file_removed: true,
                removed,
            });
        }
        write(&hooks.file, &root)?;
        return Ok(UninstallReport {
            file_removed: false,
            removed,
        });
    }
    let mut root = load(&hooks.file)?;
    let mut removed = Vec::new();
    for entry in &hooks.entries {
        if let Some(arr) = array_at_mut(&mut root, &entry.pointer) {
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

    /// Review round (#418): `install` must never coerce a non-array value
    /// that is already at an entry's own pointer -- it errors and leaves the
    /// file byte-identical.
    #[test]
    fn install_over_a_non_array_slot_errors_and_never_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = droid_hooks(dir.path());
        std::fs::create_dir_all(hooks.file.parent().unwrap()).unwrap();
        let original =
            serde_json::to_string(&json!({"PreToolUse": {"operatorSetting": true}})).unwrap();
        std::fs::write(&hooks.file, &original).unwrap();

        let err = install(&hooks).expect_err("must refuse to coerce an object into an array");
        assert!(
            err.to_string().contains("PreToolUse"),
            "error should name the pointer: {err}"
        );
        assert!(
            err.to_string().contains("expected an array"),
            "error should say what was expected: {err}"
        );

        let after = std::fs::read_to_string(&hooks.file).expect("read");
        assert_eq!(after, original, "a failed install must not touch the file");
    }

    /// Review round (#418): `uninstall` over the same non-array slot is a
    /// pure no-op (no creation, no coercion, no write) rather than an error
    /// -- there is nothing zirv-owned to remove from something that is not
    /// an array at all.
    #[test]
    fn uninstall_over_a_non_array_slot_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = droid_hooks(dir.path());
        std::fs::create_dir_all(hooks.file.parent().unwrap()).unwrap();
        let original =
            serde_json::to_string(&json!({"PreToolUse": {"operatorSetting": true}})).unwrap();
        std::fs::write(&hooks.file, &original).unwrap();

        let report = uninstall(&hooks).expect("uninstall never errors");
        assert!(!report.file_removed);
        assert!(report.removed.is_empty());

        let after = std::fs::read_to_string(&hooks.file).expect("read");
        assert_eq!(after, original, "a no-op uninstall must not touch the file");
    }

    /// Review round (#418): an `owned_file` uninstall (copilot) must not
    /// blindly delete the file when an operator's own element still lives in
    /// one of its arrays -- it filters instead, keeping everything zirv did
    /// not write.
    #[test]
    fn uninstall_on_copilot_keeps_the_file_when_an_operator_element_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks = copilot_hooks(dir.path());
        install(&hooks).expect("install");

        // An operator hand-adds their own entry to the same array zirv's
        // own guard/safety-check entries live in.
        let mut root: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks.file).expect("read"))
                .expect("json");
        root["hooks"]["preToolUse"]
            .as_array_mut()
            .expect("array")
            .push(json!({"type": "command", "bash": "./mine.sh"}));
        std::fs::write(&hooks.file, serde_json::to_string(&root).unwrap()).unwrap();

        let report = uninstall(&hooks).expect("uninstall");
        assert!(
            !report.file_removed,
            "the operator's own element must keep the file alive"
        );
        assert!(hooks.file.exists());

        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks.file).expect("read"))
                .expect("json");
        assert_eq!(after.get("version").and_then(Value::as_u64), Some(1));
        assert!(
            !value_is_zirv_owned(&after),
            "no zirv-owned element may survive: {after}"
        );
        let pre = after
            .pointer("/hooks/preToolUse")
            .and_then(Value::as_array)
            .expect("array");
        assert_eq!(pre.len(), 1, "only the operator's own element remains");
        assert_eq!(pre[0]["bash"], "./mine.sh");
    }
}
