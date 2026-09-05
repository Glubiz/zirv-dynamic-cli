//! `zirv commands [--json]` (issue #355): the command surface generated
//! straight from the clap model that already backs every subcommand, so a
//! prompt or skill pointed at this command never carries hand-copied text
//! that can drift from what the installed binary actually accepts.
//!
//! Every path, `about`, positional argument and flag is read off a real
//! `clap::Command` via [`clap::CommandFactory`] -- nothing here is retyped
//! by hand except the `mutating`/`availability` classification, which clap
//! has no way to infer on its own. Discovery only ever inspects these
//! `Command` trees; it never invokes a command, so listing the surface
//! cannot execute a mutating default (`discovery_never_executes_anything`).

use std::io::Write;

use clap::{Command, CommandFactory};
use serde::{Deserialize, Serialize};

use super::ctx::CtxResult;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgEntry {
    pub name: String,
    pub required: bool,
    pub value_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagEntry {
    pub long: Option<String>,
    pub short: Option<char>,
    pub takes_value: bool,
    pub help: Option<String>,
}

/// When a command is safe/meaningful to run. Deliberately coarse -- three
/// buckets, not a full precondition language -- because the only consumer is
/// an agent deciding whether to try a command cold: `Always` needs nothing
/// beyond the repository it is run from; `CtxSession` targets or reports on
/// an already-registered supervised session (`zirv ctx status` still counts
/// as `Always` -- it works with zero live sessions -- but `nudge`/`kill`/
/// `send`/`inbox`/`handover` act ON one); `Workflow` is the durable
/// `zirv workflow` lifecycle tree, which needs a started (or startable)
/// workflow to mean anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Availability {
    Always,
    CtxSession,
    Workflow,
}

impl Availability {
    pub fn label(self) -> &'static str {
        match self {
            Availability::Always => "always",
            Availability::CtxSession => "ctx-session",
            Availability::Workflow => "workflow",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEntry {
    pub path: String,
    pub about: String,
    pub args: Vec<ArgEntry>,
    pub flags: Vec<FlagEntry>,
    pub mutating: bool,
    pub availability: Availability,
}

/// Leaf paths (space-joined, e.g. `"zirv ctx agent"`) that only ever read or
/// report state: they never write to disk, a memory bank, a session
/// registry, or the network. Every leaf `command_entries` discovers must
/// appear in exactly this table or [`MUTATING`] --
/// `every_discovered_leaf_is_classified_exactly_once` fails the build the
/// moment a new command lands unclassified, rather than letting `mutating`
/// silently default to whichever is less safe.
const READ_ONLY: &[&str] = &[
    "zirv help",
    "zirv version",
    "zirv skill",
    "zirv commands",
    "zirv ctx score",
    "zirv ctx status",
    "zirv ctx usage",
    "zirv ctx optimize",
    "zirv ctx inbox",
    "zirv ctx recall",
    "zirv ctx safety check",
    "zirv ctx safety list",
    "zirv ctx safety explain",
    "zirv ctx permissions audit",
    "zirv ctx objective show",
    "zirv ctx group status",
    "zirv ctx compile",
    "zirv ctx spend",
    "zirv ctx snapshot",
    "zirv ctx search",
    "zirv ctx measure",
    "zirv ctx explain-status",
    "zirv ctx wait",
    "zirv ctx worktree list",
    "zirv ctx worktree finalize",
    "zirv ctx task list",
    "zirv ctx task show",
    "zirv memory status",
    "zirv memory list",
    "zirv memory recall",
    "zirv context sync",
    "zirv context lint",
    "zirv context status",
    "zirv workflow list",
    "zirv workflow show",
    "zirv workflow classify",
    "zirv workflow status",
    "zirv workflow context",
    "zirv workflow artifacts",
    "zirv workflow agents list",
    "zirv workflow agents show",
    "zirv workflow review status",
    "zirv workflow review show",
    "zirv workflow review list",
    "zirv workflow review package",
    "zirv workflow maintain scan",
    "zirv workflow stats",
    "zirv test changed",
    "zirv test all",
    "zirv verify",
    "zirv artifact list",
    "zirv artifact show",
    "zirv artifact present",
    "zirv frontend profile",
    "zirv frontend status",
    "zirv frontend check",
    "zirv frontend capabilities",
    "zirv frontend benchmark",
    "zirv setup status",
    "zirv skill list",
    "zirv skill show",
];

/// Leaf paths that write: state on disk, a session/registry entry, a
/// process, a memory bank, or an outbound network call (`zirv report`
/// files a GitHub issue, `zirv ctx permissions propose` can open or comment
/// on a GitHub issue). See [`READ_ONLY`]'s own doc comment for the
/// completeness guarantee both tables share.
const MUTATING: &[&str] = &[
    "zirv init",
    "zirv create",
    "zirv report bug",
    "zirv report feature",
    "zirv ctx chat",
    "zirv ctx agent",
    "zirv ctx handoff",
    "zirv ctx resume",
    "zirv ctx hook notify",
    "zirv ctx hook permission",
    "zirv ctx hook pre-compact",
    "zirv ctx hook pretool",
    "zirv ctx hook prompt",
    "zirv ctx hook session-start",
    "zirv ctx hook stop",
    "zirv ctx loop",
    "zirv ctx exec",
    "zirv ctx wrap",
    "zirv ctx send",
    "zirv ctx remember",
    "zirv ctx forget",
    "zirv ctx nudge",
    "zirv ctx kill",
    "zirv ctx handover",
    "zirv ctx group create",
    "zirv ctx group close",
    "zirv ctx objective set",
    "zirv ctx objective close",
    "zirv ctx permissions compile",
    "zirv ctx permissions propose",
    "zirv ctx usage tee",
    "zirv ctx worktree prune",
    "zirv ctx task create",
    "zirv ctx task claim",
    "zirv ctx task heartbeat",
    "zirv ctx task complete",
    "zirv ctx task block",
    "zirv ctx task unblock",
    "zirv ctx task comment",
    "zirv ctx task archive",
    "zirv ctx swarm",
    "zirv ctx measure baseline",
    "zirv memory init",
    "zirv memory remember",
    "zirv memory forget",
    "zirv memory verify",
    "zirv memory optimize",
    "zirv memory promote",
    "zirv memory rollback",
    "zirv workflow start",
    "zirv workflow resume",
    "zirv workflow reclassify",
    "zirv workflow approve",
    "zirv workflow advance",
    "zirv workflow close",
    "zirv workflow review run",
    "zirv workflow review dispose",
    "zirv workflow review add",
    "zirv workflow review ingest-pr-comments",
    "zirv workflow agents dispatch",
    "zirv test baseline",
    "zirv artifact render",
    "zirv frontend render",
    "zirv frontend review",
    "zirv setup",
    "zirv setup apply",
    "zirv setup profile",
    "zirv setup reset",
    "zirv setup restore",
];

fn classify(path: &str) -> Option<bool> {
    if READ_ONLY.contains(&path) {
        Some(false)
    } else if MUTATING.contains(&path) {
        Some(true)
    } else {
        None
    }
}

fn availability_for(path: &str) -> Availability {
    if path.starts_with("zirv workflow")
        || path.starts_with("zirv test")
        || path.starts_with("zirv verify")
    {
        Availability::Workflow
    } else if matches!(
        path,
        "zirv ctx nudge"
            | "zirv ctx kill"
            | "zirv ctx send"
            | "zirv ctx inbox"
            | "zirv ctx handover"
    ) {
        Availability::CtxSession
    } else {
        Availability::Always
    }
}

/// Whether `cmd` is a leaf `command_entries` should emit an entry for: it has
/// no further subcommands, or its subcommand is optional (`zirv setup`
/// itself does something when no verb is given, in addition to every verb
/// under it -- see `SetupCli::verb: Option<SetupVerb>`).
fn is_leaf(cmd: &Command) -> bool {
    cmd.get_subcommands().next().is_none() || !cmd.is_subcommand_required_set()
}

fn arg_and_flag_entries(cmd: &Command) -> (Vec<ArgEntry>, Vec<FlagEntry>) {
    let mut args = Vec::new();
    let mut flags = Vec::new();
    for arg in cmd.get_arguments() {
        let id = arg.get_id().as_str();
        if id == "help" || id == "version" {
            continue;
        }
        if arg.is_positional() {
            args.push(ArgEntry {
                name: id.to_string(),
                required: arg.is_required_set(),
                value_name: arg
                    .get_value_names()
                    .and_then(|names| names.first())
                    .map(|name| name.to_string()),
            });
        } else {
            let takes_value = !matches!(
                arg.get_action(),
                clap::ArgAction::SetTrue
                    | clap::ArgAction::SetFalse
                    | clap::ArgAction::Count
                    | clap::ArgAction::Help
                    | clap::ArgAction::HelpShort
                    | clap::ArgAction::HelpLong
                    | clap::ArgAction::Version
            );
            flags.push(FlagEntry {
                long: arg.get_long().map(|s| s.to_string()),
                short: arg.get_short(),
                takes_value,
                help: arg.get_help().map(|s| s.to_string()),
            });
        }
    }
    (args, flags)
}

/// Recursively walks `cmd` (whose own name is `prefix`, already
/// space-joined, e.g. `"zirv ctx"`), appending one [`CommandEntry`] per leaf
/// it finds -- `unclassified` collects any leaf `classify` does not
/// recognize, so a single caller (`walk_and_classify`) can report every gap
/// at once instead of panicking on the first.
fn walk(cmd: &Command, prefix: &str, out: &mut Vec<CommandEntry>, unclassified: &mut Vec<String>) {
    if is_leaf(cmd) {
        let path = prefix.to_string();
        let (args, flags) = arg_and_flag_entries(cmd);
        match classify(&path) {
            Some(mutating) => out.push(CommandEntry {
                about: cmd.get_about().map(|s| s.to_string()).unwrap_or_default(),
                availability: availability_for(&path),
                args,
                flags,
                mutating,
                path,
            }),
            None => unclassified.push(path),
        }
    }
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let path = format!("{prefix} {}", sub.get_name());
        walk(sub, &path, out, unclassified);
    }
}

/// One [`CommandEntry`] for a top-level built-in that has no `clap::Command`
/// of its own to introspect: `main.rs` intercepts these against raw argv
/// before clap ever runs (`help`/`version`) or they are hand-parsed
/// (`chat`/`agent`/`skill`/`commands`, this command's own two forms).
fn synthetic(path: &str, about: &str, mutating: bool, flags: Vec<FlagEntry>) -> CommandEntry {
    CommandEntry {
        path: path.to_string(),
        about: about.to_string(),
        args: Vec::new(),
        flags,
        mutating,
        availability: availability_for(path),
    }
}

fn json_flag() -> FlagEntry {
    FlagEntry {
        long: Some("json".to_string()),
        short: None,
        takes_value: false,
        help: Some("Emit machine-readable JSON.".to_string()),
    }
}

/// Every command this binary accepts, generated from its clap model plus a
/// handful of synthetic entries (see [`synthetic`]) for the few top-level
/// built-ins clap does not parse directly. Sorted by path for a stable,
/// deterministic listing regardless of which root contributed each entry.
///
/// `chat`/`agent` are aliases straight onto `zirv ctx chat`/`zirv ctx agent`
/// (see `main.rs`'s `top_level_ctx_alias`): rather than retyping their
/// `about`/flags by hand and risking drift, their rows are cloned from the
/// `zirv ctx` entries the walk already produced.
pub fn command_entries() -> CtxResult<Vec<CommandEntry>> {
    let mut entries = Vec::new();
    let mut unclassified = Vec::new();

    let roots: [Command; 6] = [
        super::ctx::CtxCli::command(),
        super::ctx::memory_cli::MemoryCli::command(),
        super::ctx::context_cli::ContextCli::command(),
        super::workflow::command(),
        super::report::ReportCli::command(),
        super::setup::SetupCli::command(),
    ];
    for root in &roots {
        let prefix = root.get_name().to_string();
        walk(root, &prefix, &mut entries, &mut unclassified);
    }

    if !unclassified.is_empty() {
        unclassified.sort();
        unclassified.dedup();
        return Err(format!(
            "the following commands are not classified as read-only or mutating in \
             command_schema.rs's READ_ONLY/MUTATING tables: {}",
            unclassified.join(", ")
        )
        .into());
    }

    for (alias, canonical) in [
        ("zirv chat", "zirv ctx chat"),
        ("zirv agent", "zirv ctx agent"),
    ] {
        let source = entries
            .iter()
            .find(|entry| entry.path == canonical)
            .unwrap_or_else(|| panic!("{canonical} must exist for the {alias} alias to clone"))
            .clone();
        entries.push(CommandEntry {
            path: alias.to_string(),
            ..source
        });
    }

    entries.push(synthetic(
        "zirv help",
        "List available scripts, shortcuts and built-in commands.",
        false,
        Vec::new(),
    ));
    entries.push(synthetic(
        "zirv version",
        "Print the installed zirv version.",
        false,
        Vec::new(),
    ));
    entries.push(synthetic(
        "zirv init",
        "Scaffold a .zirv directory in the current repository.",
        true,
        Vec::new(),
    ));
    entries.push(synthetic(
        "zirv create",
        "Create a new repository or global script.",
        true,
        Vec::new(),
    ));
    entries.push(synthetic(
        "zirv skill",
        "Print the bundled operator orientation skill, release-matched to \
         this binary.",
        false,
        vec![json_flag()],
    ));
    entries.push(synthetic(
        "zirv commands",
        "List every command this binary accepts, generated from its own \
         clap model.",
        false,
        vec![json_flag()],
    ));

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn render_human(entries: &[CommandEntry], writer: &mut impl Write) -> std::io::Result<()> {
    for entry in entries {
        writeln!(
            writer,
            "{:<32} [{}] ({})  {}",
            entry.path,
            if entry.mutating { "W" } else { "R" },
            entry.availability.label(),
            entry.about
        )?;
    }
    Ok(())
}

/// `args[1..]` is whatever followed `commands` in argv (just `--json`, or
/// nothing): `main.rs` intercepts `zirv commands`/`zirv commands --json`
/// against raw argv before clap ever runs, the same shape `skill.rs`'s own
/// built-in uses, so an unrecognized extra argument here is simply ignored
/// rather than mis-parsed as a script name.
pub fn dispatch(json: bool, writer: &mut impl Write) -> i32 {
    let entries = match command_entries() {
        Ok(entries) => entries,
        Err(err) => {
            crate::output::error(err);
            return 1;
        }
    };
    let result = if json {
        serde_json::to_writer_pretty(&mut *writer, &entries)
            .map_err(std::io::Error::from)
            .and_then(|_| writeln!(writer))
    } else {
        render_human(&entries, writer)
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            crate::output::error(err);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The completeness guarantee both `READ_ONLY` and `MUTATING` exist for:
    /// every leaf the walk actually discovers is classified in exactly one
    /// of the two tables, so a newly added command that nobody classified
    /// fails this test instead of silently defaulting.
    #[test]
    fn every_discovered_leaf_is_classified_exactly_once() {
        let entries = command_entries().expect("every leaf must be classified");
        assert!(!entries.is_empty());

        let mut seen = std::collections::HashSet::new();
        for entry in &entries {
            assert!(
                seen.insert(entry.path.clone()),
                "duplicate path in command_entries: {}",
                entry.path
            );
        }

        let read_only_and_mutating: std::collections::HashSet<&&str> =
            READ_ONLY.iter().collect::<std::collections::HashSet<_>>();
        for path in MUTATING {
            assert!(
                !read_only_and_mutating.contains(path),
                "'{path}' is listed in both READ_ONLY and MUTATING"
            );
        }
    }

    #[test]
    fn discovery_never_executes_anything() {
        // Building the command surface never touches the filesystem or a
        // session registry: it only ever introspects `clap::Command` trees.
        // Calling it twice from a location with no `.zirv` at all, and
        // getting the identical answer both times, is the closest a unit
        // test can get to proving "reading this never ran a mutating
        // default".
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::CwdGuard::enter(dir.path())
            .expect("enter empty tempdir");
        let first = command_entries().expect("classified");
        let second = command_entries().expect("classified");
        assert_eq!(first, second);
        assert!(!dir.path().join(".zirv").exists());
    }

    #[test]
    fn json_output_round_trips_and_is_deterministic() {
        let entries = command_entries().expect("classified");
        let first = serde_json::to_string_pretty(&entries).expect("serialize");
        let second = serde_json::to_string_pretty(&entries).expect("serialize");
        assert_eq!(first, second, "identical input must serialize identically");

        let round_tripped: Vec<CommandEntry> =
            serde_json::from_str(&first).expect("round-trip through serde");
        assert_eq!(round_tripped, entries);
    }

    #[test]
    fn ctx_agent_is_present_with_its_worktree_and_workdir_flags() {
        let entries = command_entries().expect("classified");
        let agent = entries
            .iter()
            .find(|entry| entry.path == "zirv ctx agent")
            .expect("zirv ctx agent must be discovered");
        assert!(agent.mutating, "agent dispatches a worker: it mutates");
        let long_flags: Vec<&str> = agent
            .flags
            .iter()
            .filter_map(|flag| flag.long.as_deref())
            .collect();
        assert!(
            long_flags.contains(&"worktree"),
            "got flags: {long_flags:?}"
        );
        assert!(long_flags.contains(&"workdir"), "got flags: {long_flags:?}");
    }

    #[test]
    fn report_bug_and_feature_are_mutating_they_file_a_real_github_issue() {
        let entries = command_entries().expect("classified");
        for path in ["zirv report bug", "zirv report feature"] {
            let entry = entries
                .iter()
                .find(|entry| entry.path == path)
                .unwrap_or_else(|| panic!("{path} must be discovered"));
            assert!(
                entry.mutating,
                "{path} files a real GitHub issue over HTTP: it mutates"
            );
        }
    }

    #[test]
    fn top_level_aliases_clone_their_canonical_ctx_entry() {
        let entries = command_entries().expect("classified");
        let alias = entries
            .iter()
            .find(|entry| entry.path == "zirv chat")
            .expect("zirv chat alias must be discovered");
        let canonical = entries
            .iter()
            .find(|entry| entry.path == "zirv ctx chat")
            .expect("zirv ctx chat must be discovered");
        assert_eq!(alias.about, canonical.about);
        assert_eq!(alias.mutating, canonical.mutating);
    }

    #[test]
    fn human_output_lists_one_line_per_command() {
        let entries = command_entries().expect("classified");
        let mut buffer = Vec::new();
        render_human(&entries, &mut buffer).expect("render");
        let text = String::from_utf8(buffer).expect("utf8");
        assert_eq!(text.lines().count(), entries.len());
        assert!(text.contains("zirv ctx status"));
    }
}
