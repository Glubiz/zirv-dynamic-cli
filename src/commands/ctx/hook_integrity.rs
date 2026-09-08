//! Issue #420: an integrity baseline for the hook command strings zirv
//! writes into `<claude config dir>/settings.json`
//! (`setup::HARNESS_HOOKS`/`setup::CLAUDE_ONLY_HOOKS`) and `<codex
//! home>/hooks.json` (`setup::HARNESS_HOOKS` only), a once-a-day drift
//! warning when a live entry no longer matches what zirv would write, and an
//! exact-match self-heal for an entry that still carries a byte-identical
//! previous zirv shape.
//!
//! ## Where the baseline lives
//! `StateDir::hook_baseline` (`<state>/hooks/baseline.json`) -- one JSON
//! object keyed by the *target file's own path* (the claude `settings.json`
//! path or the codex `hooks.json` path, so a `CLAUDE_CONFIG_DIR`/`CODEX_HOME`
//! override still gets its own baseline), and under that by
//! `"<event>|<matcher-or-empty>"`, to a hex SHA-256 of the exact command
//! string zirv wrote for that hook slot at install time
//! (`setup::install_claude_integration`/`install_codex_hooks`, via
//! [`record_baseline`]). Only the command string is hashed: zirv registers a
//! command, never a script file, so there is nothing else to fingerprint.
//!
//! ## Classification
//! For each hook slot the *current* binary would install
//! (`setup::HARNESS_HOOKS`/`CLAUDE_ONLY_HOOKS`), [`classify_entry`] compares
//! the live command actually found under that event+matcher in the target
//! file against the current shape, the operator's own recorded baseline hash
//! for that slot, and [`LEGACY_HOOK_SHAPES`]:
//!
//! - `Missing` -- no live command for this slot, but the operator's own
//!   baseline proves it *was* installed: a genuine regression.
//! - `Ok` -- the live command matches the current shape, and a baseline was
//!   recorded for it.
//! - `NoBaseline` -- either the live command matches the current shape but
//!   no baseline was ever recorded for it, or there is no live command *and*
//!   no baseline (never installed at all -- a fresh machine, or one that has
//!   not run `zirv setup apply` since #420 shipped) -- advisory only, never
//!   a warning trigger.
//! - `Outdated` -- the live command differs from the current shape but
//!   either matches this operator's own baseline hash for the slot (the
//!   strongest evidence: it is *exactly* what a previous zirv install wrote
//!   here) or matches a [`LEGACY_HOOK_SHAPES`] row for the same slot.
//! - `Modified` -- the live command matches neither the current shape, the
//!   baseline, nor any known legacy shape: something other than zirv edited
//!   it, or it drifted in some way this table does not recognize.
//!
//! ## Legacy shapes
//! [`LEGACY_HOOK_SHAPES`] is a small, hand-maintained table of retired
//! command strings for a hook slot that still exists in
//! `setup::HARNESS_HOOKS`/`setup::CLAUDE_ONLY_HOOKS` today. It starts empty:
//! no hook's command string has changed since this table was introduced.
//! Append a row here whenever one does, so an operator still on the previous
//! shape keeps classifying as `Outdated` (self-healable) rather than
//! `Modified`, even on a machine whose baseline predates the change.
//!
//! ## Self-heal
//! [`heal_outdated`] only ever replaces a live entry's command string when it
//! is byte-identical to a known previous zirv shape (a [`LEGACY_HOOK_SHAPES`]
//! row for that slot) -- never merely because it differs from the current
//! shape. Anything else (an operator's own hand-edit, a third-party tool's
//! entry, a slot that is simply missing) is left untouched. The write goes
//! through [`super::state::write_atomic`] (temp file + rename in the same
//! directory), so a concurrent reader never observes a half-written file, and
//! several concurrent heals converge on the same, correct output (every
//! healer computes the identical replacement for the same input).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CtxResult;
use super::memory::sha256_hex;
use super::state::{self, StateDir};
use crate::commands::setup;

/// One hook slot's identity and the command string zirv associates with it:
/// `(event, matcher, command)`, exactly the shape `setup::HARNESS_HOOKS` and
/// `setup::CLAUDE_ONLY_HOOKS` already use.
pub(crate) type HookShape = (&'static str, Option<&'static str>, &'static str);

/// Retired command shapes for a hook slot that still exists in
/// `setup::HARNESS_HOOKS`/`setup::CLAUDE_ONLY_HOOKS` today, kept solely so
/// [`classify_entry`] and [`heal_outdated`] keep recognizing an operator's
/// previous-shape entry as zirv-authored (`Outdated`, self-healable) rather
/// than unrecognized (`Modified`). Empty today: no hook's command string has
/// ever changed since this table was introduced. Append a row here --
/// `(event, matcher, old_command)` -- whenever one does; the row's `event`
/// and `matcher` must match an existing current-shape slot for `heal_
/// outdated` to have anywhere to heal it *to*.
pub(crate) const LEGACY_HOOK_SHAPES: &[HookShape] = &[];

const WARNING_INTERVAL_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookState {
    Ok,
    Outdated,
    Missing,
    Modified,
    NoBaseline,
}

impl HookState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            HookState::Ok => "ok",
            HookState::Outdated => "outdated",
            HookState::Missing => "missing",
            HookState::Modified => "modified",
            HookState::NoBaseline => "no-baseline",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HookRow {
    pub provider: &'static str,
    pub event: &'static str,
    pub matcher: Option<&'static str>,
    pub state: HookState,
}

/// `"<event>|<matcher-or-empty>"`.
pub(crate) fn matcher_suffix(matcher: Option<&str>) -> String {
    match matcher {
        Some(m) => format!("[{m}]"),
        None => String::new(),
    }
}

fn slot_key(event: &str, matcher: Option<&str>) -> String {
    format!("{event}|{}", matcher.unwrap_or(""))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Baseline {
    #[serde(default)]
    targets: BTreeMap<String, BTreeMap<String, String>>,
}

impl Baseline {
    fn load(state: &StateDir) -> Self {
        std::fs::read_to_string(state.hook_baseline())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn save(&self, state: &StateDir) -> std::io::Result<()> {
        let path = state.hook_baseline();
        if let Some(parent) = path.parent() {
            state::create_private_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string()) + "\n";
        state::write_private(&path, &body)
    }

    fn get(&self, target: &str, event: &str, matcher: Option<&str>) -> Option<&str> {
        self.targets
            .get(target)?
            .get(&slot_key(event, matcher))
            .map(String::as_str)
    }

    fn set(&mut self, target: &str, event: &str, matcher: Option<&str>, command: &str) {
        self.targets
            .entry(target.to_string())
            .or_default()
            .insert(slot_key(event, matcher), sha256_hex(command));
    }
}

/// Records a baseline hash for each `(event, matcher, command)` triple in
/// `written` -- the entries `setup::install_claude_integration`/
/// `install_codex_hooks` actually just wrote into `target` -- keyed by
/// `target`'s own path. Every production caller treats a failure here as
/// non-fatal: a missing baseline degrades to `HookState::NoBaseline`
/// everywhere it is read, never a hard error.
pub(crate) fn record_baseline(
    state: &StateDir,
    target: &Path,
    written: &[HookShape],
) -> std::io::Result<()> {
    if written.is_empty() {
        return Ok(());
    }
    let mut baseline = Baseline::load(state);
    let target_key = target.to_string_lossy().to_string();
    for (event, matcher, command) in written {
        baseline.set(&target_key, event, *matcher, command);
    }
    baseline.save(state)
}

struct Target {
    provider: &'static str,
    path: PathBuf,
    shapes: Vec<HookShape>,
}

fn targets_for(home: &Path) -> Vec<Target> {
    let mut claude_shapes: Vec<HookShape> = setup::HARNESS_HOOKS.to_vec();
    claude_shapes.extend(setup::CLAUDE_ONLY_HOOKS);
    vec![
        Target {
            provider: "claude",
            path: setup::claude_config_dir(home).join("settings.json"),
            shapes: claude_shapes,
        },
        Target {
            provider: "codex",
            path: setup::codex_config_dir(home).join("hooks.json"),
            shapes: setup::HARNESS_HOOKS.to_vec(),
        },
    ]
}

/// Every command string zirv wrote under `settings["hooks"][event]` for
/// entries whose `matcher` field equals `matcher` (or is absent, when
/// `matcher` is `None`) -- not just zirv's own entries; an operator's own
/// unrelated hook sharing this exact matcher would show up here too, which
/// [`resolve_live_command`] accounts for by preferring a recognized shape.
fn live_commands_for(settings: &Value, event: &str, matcher: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(entries) = settings
        .get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(Value::as_array)
    else {
        return out;
    };
    for entry in entries {
        if entry.get("matcher").and_then(Value::as_str) != matcher {
            continue;
        }
        let Some(hooks) = entry.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for hook in hooks {
            if hook.get("type").and_then(Value::as_str) != Some("command") {
                continue;
            }
            if let Some(command) = hook.get("command").and_then(Value::as_str) {
                out.push(command.to_string());
            }
        }
    }
    out
}

/// Picks the one live command that best represents this slot: the current
/// shape if present, else a recognized legacy shape, else whatever else is
/// there (unrecognized -- `classify_entry` reports that as `Modified`), else
/// `None` (`Missing`).
fn resolve_live_command(
    settings: &Value,
    event: &str,
    matcher: Option<&str>,
    current_command: &str,
    legacy_commands: &[&str],
) -> Option<String> {
    let commands = live_commands_for(settings, event, matcher);
    if commands.iter().any(|c| c == current_command) {
        return Some(current_command.to_string());
    }
    for legacy in legacy_commands {
        if commands.iter().any(|c| c == legacy) {
            return Some((*legacy).to_string());
        }
    }
    commands.into_iter().next()
}

fn legacy_commands_for<'a>(
    event: &str,
    matcher: Option<&str>,
    current_command: &str,
    legacy_shapes: &'a [HookShape],
) -> Vec<&'a str> {
    legacy_shapes
        .iter()
        .filter(|(e, m, c)| *e == event && *m == matcher && *c != current_command)
        .map(|(_, _, c)| *c)
        .collect()
}

fn classify_entry(
    settings: &Value,
    baseline: &Baseline,
    target_key: &str,
    event: &str,
    matcher: Option<&str>,
    current_command: &str,
    legacy_commands: &[&str],
) -> HookState {
    let baseline_hash = baseline.get(target_key, event, matcher);
    let Some(live) =
        resolve_live_command(settings, event, matcher, current_command, legacy_commands)
    else {
        // No live command *and* nothing was ever baselined for this slot --
        // this is "never installed" (a fresh machine, or one that has not
        // run `zirv setup apply` since #420 shipped), not a regression.
        // `Missing` is reserved for a slot the operator's own baseline
        // proves was installed and has since disappeared.
        return if baseline_hash.is_some() {
            HookState::Missing
        } else {
            HookState::NoBaseline
        };
    };
    if live == current_command {
        return if baseline_hash.is_some() {
            HookState::Ok
        } else {
            HookState::NoBaseline
        };
    }
    let live_hash = sha256_hex(&live);
    if baseline_hash == Some(live_hash.as_str()) {
        return HookState::Outdated;
    }
    if legacy_commands.contains(&live.as_str()) {
        return HookState::Outdated;
    }
    HookState::Modified
}

fn report_with_legacy(
    state: &StateDir,
    home: &Path,
    legacy_shapes: &[HookShape],
) -> CtxResult<Vec<HookRow>> {
    let baseline = Baseline::load(state);
    let mut rows = Vec::new();
    for target in targets_for(home) {
        let settings = setup::load_json_object(&target.path)?;
        let target_key = target.path.to_string_lossy().to_string();
        for (event, matcher, current_command) in &target.shapes {
            let legacy_commands =
                legacy_commands_for(event, *matcher, current_command, legacy_shapes);
            let state = classify_entry(
                &settings,
                &baseline,
                &target_key,
                event,
                *matcher,
                current_command,
                &legacy_commands,
            );
            rows.push(HookRow {
                provider: target.provider,
                event,
                matcher: *matcher,
                state,
            });
        }
    }
    Ok(rows)
}

/// `zirv ctx hook status`'s underlying report: one row per hook slot the
/// current binary would install, across both the claude and codex targets.
pub(crate) fn report(state: &StateDir, home: &Path) -> CtxResult<Vec<HookRow>> {
    report_with_legacy(state, home, LEGACY_HOOK_SHAPES)
}

pub(crate) fn any_drift(rows: &[HookRow]) -> bool {
    rows.iter().any(|row| {
        matches!(
            row.state,
            HookState::Outdated | HookState::Missing | HookState::Modified
        )
    })
}

fn summarize(rows: &[HookRow]) -> String {
    let bad: Vec<String> = rows
        .iter()
        .filter(|row| {
            matches!(
                row.state,
                HookState::Outdated | HookState::Missing | HookState::Modified
            )
        })
        .map(|row| {
            format!(
                "{} {}{}: {}",
                row.provider,
                row.event,
                matcher_suffix(row.matcher),
                row.state.label()
            )
        })
        .collect();
    format!(
        "{} hook entr{} need attention ({}); run `zirv ctx hook status` for detail",
        bad.len(),
        if bad.len() == 1 { "y" } else { "ies" },
        bad.join(", ")
    )
}

/// `true` when the once-a-day marker (`StateDir::hook_warn_marker`) is
/// absent, unreadable, or older than [`WARNING_INTERVAL_SECS`]. Fails open in
/// every direction: a marker this call cannot inspect at all is treated as
/// due, same as one that is simply old.
pub(crate) fn warning_due(state: &StateDir) -> bool {
    match std::fs::metadata(state.hook_warn_marker()).and_then(|meta| meta.modified()) {
        Ok(modified) => match modified.elapsed() {
            Ok(elapsed) => elapsed.as_secs() >= WARNING_INTERVAL_SECS,
            Err(_) => true, // clock skew (mtime in the future) -- fail open
        },
        Err(_) => true,
    }
}

/// Best-effort: a marker that fails to write only costs an extra warning
/// next time, never a crash.
fn record_warning(state: &StateDir) {
    let marker = state.hook_warn_marker();
    if let Some(parent) = marker.parent() {
        let _ = state::create_private_dir_all(parent);
    }
    let _ = state::write_private(&marker, "");
}

/// Checks whether an integrity warning is due (`warning_due`) and, if so and
/// something actually drifted (`any_drift`), records the marker and returns
/// the one-line summary to print -- callers decide how (an `Announcer` event
/// at supervisor start, a plain report line for `ctx status`). Fails open:
/// any error resolving/reading the target settings is treated as "nothing to
/// say" rather than surfaced, since this is advisory only.
pub(crate) fn drift_warning_if_due(state: &StateDir, home: &Path) -> Option<String> {
    if !warning_due(state) {
        return None;
    }
    let rows = report(state, home).ok()?;
    if !any_drift(&rows) {
        return None;
    }
    record_warning(state);
    Some(summarize(&rows))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HealSummary {
    pub healed: usize,
}

/// Replaces every hook entry under `event`/`matcher` whose command is one of
/// `legacy_commands` with `current_command`, in place -- nothing else about
/// the entry (or the rest of `settings`) is touched. Returns how many
/// entries were replaced.
fn heal_settings_entries(
    settings: &mut Value,
    event: &str,
    matcher: Option<&str>,
    current_command: &str,
    legacy_commands: &[&str],
) -> usize {
    let mut healed = 0;
    let Some(entries) = settings
        .get_mut("hooks")
        .and_then(|hooks| hooks.get_mut(event))
        .and_then(Value::as_array_mut)
    else {
        return 0;
    };
    for entry in entries.iter_mut() {
        let entry_matcher = entry
            .get("matcher")
            .and_then(Value::as_str)
            .map(str::to_string);
        if entry_matcher.as_deref() != matcher {
            continue;
        }
        let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        for hook in hooks.iter_mut() {
            if hook.get("type").and_then(Value::as_str) != Some("command") {
                continue;
            }
            let Some(command) = hook
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if legacy_commands.contains(&command.as_str()) {
                hook["command"] = Value::String(current_command.to_string());
                healed += 1;
            }
        }
    }
    healed
}

/// Heals every self-healable slot in one target file, writing the result
/// atomically (temp file + rename in the same directory via
/// `state::write_atomic`) only when at least one entry actually changed.
/// A no-op -- and never even opens `target` -- when nothing in `shapes` has
/// a legacy shape to heal from, so a target that was never installed (and
/// does not exist on disk) is never an error while `LEGACY_HOOK_SHAPES` is
/// empty.
fn heal_target(
    target: &Path,
    shapes: &[HookShape],
    legacy_shapes: &[HookShape],
) -> CtxResult<usize> {
    let has_legacy_shape = shapes.iter().any(|(event, matcher, current)| {
        legacy_shapes
            .iter()
            .any(|(e, m, c)| e == event && m == matcher && c != current)
    });
    if !has_legacy_shape || !target.is_file() {
        return Ok(0);
    }
    let mut settings = setup::load_json_object(target)?;
    let mut healed = 0;
    for (event, matcher, current_command) in shapes {
        let legacy_commands = legacy_commands_for(event, *matcher, current_command, legacy_shapes);
        if legacy_commands.is_empty() {
            continue;
        }
        healed += heal_settings_entries(
            &mut settings,
            event,
            *matcher,
            current_command,
            &legacy_commands,
        );
    }
    if healed > 0 {
        let body = serde_json::to_string_pretty(&settings)? + "\n";
        state::write_atomic(target, &body, false)?;
    }
    Ok(healed)
}

fn heal_outdated_with_legacy(
    state: &StateDir,
    home: &Path,
    legacy_shapes: &[HookShape],
) -> CtxResult<HealSummary> {
    let mut total = 0;
    let mut baseline = Baseline::load(state);
    let mut baseline_dirty = false;
    for target in targets_for(home) {
        let healed = heal_target(&target.path, &target.shapes, legacy_shapes)?;
        if healed == 0 {
            continue;
        }
        total += healed;
        // Re-baseline every slot in this target that now reads as the
        // current shape -- the ones `heal_target` just fixed, and any that
        // already matched current but had never been baselined
        // (`NoBaseline`). A slot that is still `Missing`/`Modified` after
        // the heal (nothing in `LEGACY_HOOK_SHAPES` matched it) is left
        // alone here too, exactly as `heal_target` left it alone on disk.
        let settings = setup::load_json_object(&target.path)?;
        let target_key = target.path.to_string_lossy().to_string();
        for (event, matcher, current_command) in &target.shapes {
            let commands = live_commands_for(&settings, event, *matcher);
            if commands.iter().any(|c| c == current_command) {
                baseline.set(&target_key, event, *matcher, current_command);
                baseline_dirty = true;
            }
        }
    }
    if baseline_dirty {
        // Best-effort: the heal already landed on disk; a baseline write
        // failure here only means the next `hook status` still calls the
        // freshly-healed slot `NoBaseline` instead of `Ok`.
        let _ = baseline.save(state);
    }
    Ok(HealSummary { healed: total })
}

/// `zirv ctx hook status --heal`, and the automatic heal at supervisor
/// start: replaces every `Outdated` entry (byte-identical to a known
/// previous zirv shape) with the current binary's own shape, across both
/// the claude and codex targets.
pub(crate) fn heal_outdated(state: &StateDir, home: &Path) -> CtxResult<HealSummary> {
    heal_outdated_with_legacy(state, home, LEGACY_HOOK_SHAPES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state_at(dir: &Path) -> StateDir {
        StateDir::from_root(dir.to_path_buf())
    }

    const CURRENT: HookShape = ("Stop", None, "zirv ctx hook stop");
    const LEGACY: HookShape = ("Stop", None, "zirv-ctx hook stop");
    const OTHER_CURRENT: HookShape = ("PreToolUse", Some("Agent|Task"), "zirv ctx hook pretool");

    fn shapes() -> Vec<HookShape> {
        vec![CURRENT, OTHER_CURRENT]
    }

    fn legacy_table() -> Vec<HookShape> {
        vec![LEGACY]
    }

    fn write_settings(path: &Path, value: &Value) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, serde_json::to_string_pretty(value).expect("json")).expect("write");
    }

    fn settings_with(event: &str, matcher: Option<&str>, command: &str) -> Value {
        let mut entry = json!({"hooks": [{"type": "command", "command": command}]});
        if let Some(matcher) = matcher {
            entry["matcher"] = Value::String(matcher.to_string());
        }
        json!({"hooks": {event: [entry]}})
    }

    #[test]
    fn no_live_command_and_a_recorded_baseline_classifies_as_missing() {
        let mut baseline = Baseline::default();
        baseline.set("target", "Stop", None, "zirv ctx hook stop");
        let settings = json!({});
        let state = classify_entry(
            &settings,
            &baseline,
            "target",
            "Stop",
            None,
            "zirv ctx hook stop",
            &[],
        );
        assert_eq!(state, HookState::Missing);
    }

    /// Never installed at all (a fresh machine, or one that predates #420):
    /// no live command and nothing was ever baselined. This must read as
    /// `NoBaseline`, not `Missing` -- `Missing` is reserved for a slot the
    /// baseline proves was actually installed and has since disappeared, so
    /// a plain "never set up" machine does not look like a regression.
    #[test]
    fn no_live_command_and_no_baseline_classifies_as_no_baseline() {
        let baseline = Baseline::default();
        let settings = json!({});
        let state = classify_entry(
            &settings,
            &baseline,
            "target",
            "Stop",
            None,
            "zirv ctx hook stop",
            &[],
        );
        assert_eq!(state, HookState::NoBaseline);
    }

    #[test]
    fn a_live_command_matching_current_shape_with_a_baseline_is_ok() {
        let mut baseline = Baseline::default();
        baseline.set("target", "Stop", None, "zirv ctx hook stop");
        let settings = settings_with("Stop", None, "zirv ctx hook stop");
        let state = classify_entry(
            &settings,
            &baseline,
            "target",
            "Stop",
            None,
            "zirv ctx hook stop",
            &[],
        );
        assert_eq!(state, HookState::Ok);
    }

    #[test]
    fn a_live_command_matching_current_shape_with_no_baseline_is_no_baseline() {
        let baseline = Baseline::default();
        let settings = settings_with("Stop", None, "zirv ctx hook stop");
        let state = classify_entry(
            &settings,
            &baseline,
            "target",
            "Stop",
            None,
            "zirv ctx hook stop",
            &[],
        );
        assert_eq!(state, HookState::NoBaseline);
    }

    #[test]
    fn a_live_command_matching_a_recorded_baseline_but_not_current_is_outdated() {
        let mut baseline = Baseline::default();
        baseline.set("target", "Stop", None, "zirv-ctx hook stop");
        let settings = settings_with("Stop", None, "zirv-ctx hook stop");
        let state = classify_entry(
            &settings,
            &baseline,
            "target",
            "Stop",
            None,
            "zirv ctx hook stop",
            &["zirv-ctx hook stop"],
        );
        assert_eq!(state, HookState::Outdated);
    }

    #[test]
    fn a_live_command_matching_a_known_legacy_shape_with_no_baseline_is_outdated() {
        let baseline = Baseline::default();
        let settings = settings_with("Stop", None, "zirv-ctx hook stop");
        let state = classify_entry(
            &settings,
            &baseline,
            "target",
            "Stop",
            None,
            "zirv ctx hook stop",
            &["zirv-ctx hook stop"],
        );
        assert_eq!(state, HookState::Outdated);
    }

    #[test]
    fn a_live_command_matching_nothing_known_is_modified() {
        let baseline = Baseline::default();
        let settings = settings_with("Stop", None, "some other command entirely");
        let state = classify_entry(
            &settings,
            &baseline,
            "target",
            "Stop",
            None,
            "zirv ctx hook stop",
            &[],
        );
        assert_eq!(state, HookState::Modified);
    }

    #[test]
    fn a_baseline_that_disagrees_with_an_unrecognized_live_command_is_still_modified() {
        // The baseline recorded one command, but the live command is neither
        // that baseline value, the current shape, nor a legacy shape:
        // genuinely unrecognized, not "outdated".
        let mut baseline = Baseline::default();
        baseline.set("target", "Stop", None, "zirv ctx hook stop");
        let settings = settings_with("Stop", None, "hand-edited nonsense");
        let state = classify_entry(
            &settings,
            &baseline,
            "target",
            "Stop",
            None,
            "zirv ctx hook stop",
            &[],
        );
        assert_eq!(state, HookState::Modified);
    }

    /// Sets `CLAUDE_CONFIG_DIR`/`CODEX_HOME` for the duration of a test,
    /// restoring both on drop (including on a panicking assertion) via the
    /// shared `testenv::VarGuard` -- the same guard the supervisor tests use
    /// for identical reasons.
    fn config_dirs_at(home: &Path) -> crate::commands::ctx::testenv::VarGuard {
        crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "CLAUDE_CONFIG_DIR",
                Some(home.join(".claude").to_str().expect("utf8 path")),
            ),
            (
                "CODEX_HOME",
                Some(home.join(".codex").to_str().expect("utf8 path")),
            ),
        ])
    }

    #[test]
    fn report_covers_every_current_shape_slot_across_both_targets() {
        let repo = tempfile::tempdir().expect("state dir");
        let home = tempfile::tempdir().expect("home dir");
        let state = state_at(repo.path());

        let _guard = config_dirs_at(home.path());
        let rows = report_with_legacy(&state, home.path(), &[]).expect("report");

        let expected = setup::HARNESS_HOOKS.len() * 2 + setup::CLAUDE_ONLY_HOOKS.len();
        assert_eq!(rows.len(), expected, "got {rows:?}");
        // Never installed and never baselined: `NoBaseline`, not `Missing`
        // -- see `no_live_command_and_no_baseline_classifies_as_no_baseline`.
        assert!(rows.iter().all(|row| row.state == HookState::NoBaseline));
    }

    #[test]
    fn any_drift_is_false_when_every_row_is_ok_or_no_baseline() {
        let rows = vec![
            HookRow {
                provider: "claude",
                event: "Stop",
                matcher: None,
                state: HookState::Ok,
            },
            HookRow {
                provider: "claude",
                event: "Prompt",
                matcher: None,
                state: HookState::NoBaseline,
            },
        ];
        assert!(!any_drift(&rows));
    }

    #[test]
    fn any_drift_is_true_for_outdated_missing_or_modified() {
        for state in [HookState::Outdated, HookState::Missing, HookState::Modified] {
            let rows = vec![HookRow {
                provider: "claude",
                event: "Stop",
                matcher: None,
                state,
            }];
            assert!(any_drift(&rows), "{state:?} must count as drift");
        }
    }

    #[test]
    fn a_fresh_marker_is_not_due_but_an_old_one_is() {
        let dir = tempfile::tempdir().expect("state dir");
        let state = state_at(dir.path());
        assert!(warning_due(&state), "no marker at all is due");

        record_warning(&state);
        assert!(!warning_due(&state), "a marker just written is not due");

        let marker = state.hook_warn_marker();
        let file = std::fs::File::options()
            .write(true)
            .open(&marker)
            .expect("open marker");
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600);
        file.set_modified(stale).expect("backdate marker");
        assert!(warning_due(&state), "a 25h-old marker is due again");
    }

    #[test]
    fn drift_warning_if_due_only_fires_once_within_the_window_and_only_on_real_drift() {
        let repo = tempfile::tempdir().expect("state dir");
        let home = tempfile::tempdir().expect("home dir");
        let state = state_at(repo.path());
        let settings_path = home.path().join(".claude/settings.json");
        write_settings(&settings_path, &settings_with("Stop", None, "hand-edited"));

        let _guard = config_dirs_at(home.path());
        let first = drift_warning_if_due(&state, home.path());
        assert!(first.is_some(), "a modified entry must warn once");
        assert!(first.unwrap().contains("modified"));

        let second = drift_warning_if_due(&state, home.path());
        assert!(second.is_none(), "must not warn again inside the window");
    }

    #[test]
    fn a_known_legacy_entry_is_healed_and_the_rest_of_the_file_is_untouched() {
        let home = tempfile::tempdir().expect("home");
        let target = home.path().join("settings.json");
        write_settings(
            &target,
            &json!({
                "permissions": {"allow": ["Read"]},
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": "zirv-ctx hook stop"}]}],
                    "PreToolUse": [{
                        "matcher": "Agent|Task",
                        "hooks": [{"type": "command", "command": "zirv ctx hook pretool"}]
                    }],
                }
            }),
        );

        let healed = heal_target(&target, &shapes(), &legacy_table()).expect("heal");
        assert_eq!(healed, 1);

        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).expect("read")).expect("json");
        assert_eq!(after["permissions"]["allow"][0], "Read");
        assert_eq!(
            after["hooks"]["Stop"][0]["hooks"][0]["command"],
            "zirv ctx hook stop"
        );
        assert_eq!(
            after["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "zirv ctx hook pretool"
        );
    }

    #[test]
    fn heal_target_never_touches_an_unrecognized_command() {
        let home = tempfile::tempdir().expect("home");
        let target = home.path().join("settings.json");
        let original = json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "hand-edited"}]}]}
        });
        write_settings(&target, &original);

        let healed = heal_target(&target, &shapes(), &legacy_table()).expect("heal");
        assert_eq!(healed, 0);
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).expect("read")).expect("json");
        assert_eq!(after, original);
    }

    #[test]
    fn heal_target_is_a_no_op_on_a_missing_file_when_nothing_could_heal_it() {
        let home = tempfile::tempdir().expect("home");
        let target = home.path().join("does-not-exist.json");
        let healed = heal_target(&target, &shapes(), &[]).expect("heal");
        assert_eq!(healed, 0);
        assert!(!target.exists());
    }

    #[test]
    fn eight_concurrent_heals_converge_to_one_correct_file() {
        let home = tempfile::tempdir().expect("home");
        let target = home.path().join("settings.json");
        write_settings(
            &target,
            &json!({
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": "zirv-ctx hook stop"}]}],
                }
            }),
        );

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let target = target.clone();
                scope.spawn(move || {
                    heal_target(&target, &shapes(), &legacy_table()).expect("heal");
                });
            }
        });

        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).expect("read")).expect("json");
        assert_eq!(
            after["hooks"]["Stop"][0]["hooks"][0]["command"],
            "zirv ctx hook stop"
        );
    }

    #[test]
    fn heal_outdated_re_baselines_the_slot_it_just_fixed() {
        let repo = tempfile::tempdir().expect("state dir");
        let home = tempfile::tempdir().expect("home dir");
        let state = state_at(repo.path());
        let settings_path = home.path().join(".claude/settings.json");
        write_settings(
            &settings_path,
            &settings_with("Stop", None, "zirv-ctx hook stop"),
        );

        let _guard = config_dirs_at(home.path());
        let summary =
            heal_outdated_with_legacy(&state, home.path(), &legacy_table()).expect("heal");
        assert_eq!(summary.healed, 1);

        let rows = report_with_legacy(&state, home.path(), &legacy_table()).expect("report");
        let stop_row = rows
            .iter()
            .find(|row| row.provider == "claude" && row.event == "Stop")
            .expect("Stop row");
        assert_eq!(
            stop_row.state,
            HookState::Ok,
            "healed and re-baselined: {rows:?}"
        );
    }

    #[test]
    fn record_baseline_round_trips_through_get() {
        let dir = tempfile::tempdir().expect("state dir");
        let state = state_at(dir.path());
        record_baseline(&state, Path::new("/some/settings.json"), &[CURRENT]).expect("record");
        let baseline = Baseline::load(&state);
        assert_eq!(
            baseline.get("/some/settings.json", "Stop", None),
            Some(sha256_hex("zirv ctx hook stop")).as_deref()
        );
    }

    #[test]
    fn record_baseline_is_a_no_op_for_an_empty_written_list() {
        let dir = tempfile::tempdir().expect("state dir");
        let state = state_at(dir.path());
        record_baseline(&state, Path::new("/x"), &[]).expect("no-op");
        assert!(!state.hook_baseline().exists());
    }
}
