use std::{fs, io::Write, path::Path, path::PathBuf};

use crate::style::{self, Tone};
use crate::utils::{
    COMMANDS_DIR_NAME, SCRIPT_DIR_NAME, SUPPORTED_EXTENSIONS, Shortcuts, home_dir,
    is_reserved_command, is_reserved_zirv_file, parse_script_content,
};

/// Note appended when a script or shortcut name collides with a built-in
/// command (see `utils::RESERVED_COMMANDS`): `main.rs` matches built-ins
/// before ever looking at `.zirv/`, so that name can never be invoked.
const SHADOWED_NOTE: &str = "  (shadowed by a built-in command, unreachable)";

/// Bold section-header line (blank line, then the title painted bold) --
/// the same shape `status.rs`/`context_status.rs` use for their own section
/// headers (issue #202: one visual language everywhere).
fn header(colour: bool, title: &str) -> String {
    format!("\n{}", style::paint(title, Tone::Emphasis, colour))
}

/// Bold inline field label (e.g. `Name:`, `Description:`) -- no leading
/// blank line.
fn label(colour: bool, title: &str) -> String {
    style::paint(title, Tone::Emphasis, colour)
}

fn write_scripts<W: Write>(
    writer: &mut W,
    dir: &Path,
    colour: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file()
            && let Some(ext) = path.extension().and_then(|s| s.to_str())
            && SUPPORTED_EXTENSIONS.contains(&ext)
            && !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_reserved_zirv_file)
        {
            let content = fs::read_to_string(&path)?;
            let script = parse_script_content(&content, ext)?;

            let file_name = path.file_name().unwrap().to_string_lossy();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let shadowed = if is_reserved_command(stem) {
                style::paint(SHADOWED_NOTE, Tone::Warn, colour)
            } else {
                String::new()
            };
            writeln!(writer, "-------------------------------------------------")?;
            writeln!(
                writer,
                "{} {}{shadowed}",
                label(colour, "File:"),
                style::paint(&file_name, Tone::Accent, colour)
            )?;
            writeln!(writer, "  {} {}", label(colour, "Name:"), script.name)?;
            if let Some(desc) = script.description {
                writeln!(
                    writer,
                    "  {} {}",
                    label(colour, "Description:"),
                    style::paint(&desc, Tone::Muted, colour)
                )?;
            }
            if let Some(params) = &script.params {
                writeln!(writer, "  {}", label(colour, "Required Parameters:"))?;
                for param in params {
                    writeln!(writer, "    {}", style::paint(param, Tone::Muted, colour))?;
                }
            }
        }
    }

    Ok(())
}

fn write_shortcuts<W: Write>(
    writer: &mut W,
    dir: &Path,
    colour: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let shortcuts_path = dir.join(".shortcuts.yaml");
    if shortcuts_path.exists() {
        let content = fs::read_to_string(shortcuts_path)?;
        let shortcuts: Shortcuts = serde_yaml_ng::from_str(&content)?;
        for (key, value) in shortcuts.shortcuts {
            let shadowed = if is_reserved_command(&key) {
                style::paint(SHADOWED_NOTE, Tone::Warn, colour)
            } else {
                String::new()
            };
            writeln!(
                writer,
                "  {} -> {}{shadowed}",
                style::paint(&key, Tone::Accent, colour),
                style::paint(&value, Tone::Muted, colour)
            )?;
        }
    }
    Ok(())
}

/// One row of a name/description table: `desc` may span more than one line
/// (e.g. `ctx`'s verb list), in which case every line after the first is
/// printed indented under the description column, with no name of its own.
struct Row {
    name: &'static str,
    desc: &'static [&'static str],
}

/// Prints `rows` as a name/description table: names painted `Tone::Accent`,
/// descriptions `Tone::Muted`, names padded (by display width, so a
/// wide-character name would still line up) to the width of the widest name
/// in the table -- "aligned columns" per issue #202's design, rather than
/// the hand-typed, inconsistently-aligned spacing this replaced.
fn write_table<W: Write>(
    writer: &mut W,
    rows: &[Row],
    colour: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name_width = rows
        .iter()
        .map(|row| style::display_width(row.name))
        .max()
        .unwrap_or(0);
    for row in rows {
        let pad = " ".repeat(name_width.saturating_sub(style::display_width(row.name)));
        for (i, line) in row.desc.iter().enumerate() {
            if i == 0 {
                writeln!(
                    writer,
                    "  {}{pad}  {}",
                    style::paint(row.name, Tone::Accent, colour),
                    style::paint(line, Tone::Muted, colour)
                )?;
            } else {
                writeln!(
                    writer,
                    "  {}  {}",
                    " ".repeat(name_width),
                    style::paint(line, Tone::Muted, colour)
                )?;
            }
        }
    }
    Ok(())
}

/// Usage, flags and the built-in commands: everything true of zirv regardless
/// of what is in the current directory. Intercepting `--help` took clap's
/// generated help away, and what replaced it printed nothing at all in a
/// directory with no scripts -- so the flags it documents were discoverable
/// from no help output anywhere.
fn write_builtins<W: Write>(
    writer: &mut W,
    colour: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(writer, "zirv {}\n", env!("CARGO_PKG_VERSION"))?;
    writeln!(
        writer,
        "Usage: zirv <script|command> [params...] [options]\n"
    )?;
    writeln!(
        writer,
        "Bare `zirv` (no arguments) starts `zirv ctx chat` when a local `.zirv` directory"
    )?;
    writeln!(
        writer,
        "exists here and stdin and stdout are both real terminals; otherwise it shows this help."
    )?;
    writeln!(
        writer,
        "The first time zirv (or `zirv chat`) is run in a real terminal before it has ever"
    )?;
    writeln!(
        writer,
        "been configured, a guided setup wizard runs first -- see `zirv setup`."
    )?;
    writeln!(writer, "{}", header(colour, "Commands:"))?;
    write_table(
        writer,
        &[
            Row {
                name: "help, h",
                desc: &["Show this help"],
            },
            Row {
                name: "version, v",
                desc: &["Print the version"],
            },
            Row {
                name: "init, i",
                desc: &["Create a .zirv directory here"],
            },
            Row {
                name: "create, c",
                desc: &["Create a new script"],
            },
            Row {
                name: "report",
                desc: &["File a Zirv bug or feature request on GitHub"],
            },
            Row {
                name: "setup",
                desc: &["Configure AI context, memory, hooks, migration, and safe resets"],
            },
            Row {
                name: "skill",
                desc: &["Inspect model-agnostic engineering skills"],
            },
            Row {
                name: "workflow",
                desc: &["Run and inspect development workflows"],
            },
            Row {
                name: "test",
                desc: &["Run repository-aware verification checks"],
            },
            Row {
                name: "verify",
                desc: &["Run final repository verification"],
            },
            Row {
                name: "artifact",
                desc: &["Register and inspect workflow artifacts"],
            },
            Row {
                name: "frontend",
                desc: &["Inspect autonomous frontend quality state"],
            },
            Row {
                name: "ctx",
                desc: &[
                    "Context management (score, loop, exec, wrap, handoff, resume,",
                    "hook, status, usage, optimize, chat, agent, send, inbox)",
                ],
            },
            Row {
                name: "memory",
                desc: &[
                    "Manage the memory bank without an AI session (init, status, list, recall,",
                    "remember, forget, verify)",
                ],
            },
            Row {
                name: "chat",
                desc: &["Alias for `zirv ctx chat`: start an interactive orchestrator session"],
            },
            Row {
                name: "agent <name> <prompt>",
                desc: &["Alias for `zirv ctx agent`: delegate one task to another harness"],
            },
        ],
        colour,
    )?;
    writeln!(writer, "{}", header(colour, "Options:"))?;
    write_table(
        writer,
        &[
            Row {
                name: "--dry-run",
                desc: &["Print each step instead of running it"],
            },
            Row {
                name: "-h, --help",
                desc: &["Show this help"],
            },
        ],
        colour,
    )?;
    writeln!(writer, "{}", header(colour, "create only:"))?;
    write_table(
        writer,
        &[
            Row {
                name: "--name <NAME>",
                desc: &["Script name; skips the interactive prompt"],
            },
            Row {
                name: "--shortcut <K>",
                desc: &["Shortcut key; skips the prompt"],
            },
            Row {
                name: "--global [b]",
                desc: &["Create in ~/.zirv (bare means true)"],
            },
        ],
        colour,
    )?;
    Ok(())
}

pub fn show_help<W: Write>(writer: &mut W, colour: bool) -> Result<(), Box<dyn std::error::Error>> {
    let base_dir = PathBuf::from(SCRIPT_DIR_NAME);

    write_builtins(writer, colour)?;

    if base_dir.exists() {
        // Issue #212: scripts live in `.zirv/commands/` as of zirv 3.0;
        // `.shortcuts.yaml` stays at the `.zirv` root -- it is config, not a
        // script. The two are listed independently: a root with only
        // shortcuts (no commands/ yet) still shows them, and vice versa.
        let commands_dir = base_dir.join(COMMANDS_DIR_NAME);
        if commands_dir.is_dir() {
            writeln!(writer, "{}", header(colour, "Available Scripts:"))?;
            write_scripts(writer, &commands_dir, colour)?;
        }

        let shortcuts_path = base_dir.join(".shortcuts.yaml");
        if shortcuts_path.exists() {
            writeln!(writer, "{}", header(colour, "Available Shortcuts:"))?;
            write_shortcuts(writer, &base_dir, colour)?;
        }
    }

    let root = home_dir()?.join(SCRIPT_DIR_NAME);

    if root.exists() {
        writeln!(writer, "{}", header(colour, "Global Base Scripts:"))?;
        writeln!(
            writer,
            "{}",
            style::paint(
                "Global scripts are overwritten by above mentioned scripts if they share name.",
                Tone::Muted,
                colour
            )
        )?;
        writeln!(
            writer,
            "{}",
            style::paint(&format!("Home Directory: {root:?}"), Tone::Muted, colour)
        )?;
        let commands_dir = root.join(COMMANDS_DIR_NAME);
        if commands_dir.is_dir() {
            write_scripts(writer, &commands_dir, colour)?;
        }

        let shortcuts_path = root.join(".shortcuts.yaml");
        if shortcuts_path.exists() {
            writeln!(writer, "{}", header(colour, "Global Shortcuts:"))?;
            write_shortcuts(writer, &root, colour)?;
        }
    } else {
        writeln!(
            writer,
            "{}",
            style::paint(
                &format!("No scripts found. Please create a .zirv directory in {root:?}."),
                Tone::Muted,
                colour
            )
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    /// Helper function to create a temporary .zirv directory with optional files.
    fn setup_zirv_dir(temp_dir: &Path) -> PathBuf {
        let zirv_dir = temp_dir.join(".zirv");
        create_dir_all(&zirv_dir).unwrap();
        zirv_dir
    }

    /// Issue #212: scripts now live in `.zirv/commands/`, not the `.zirv`
    /// root. Creates that subdirectory (if needed) and returns its path.
    fn setup_commands_dir(zirv_dir: &Path) -> PathBuf {
        let commands_dir = zirv_dir.join(COMMANDS_DIR_NAME);
        create_dir_all(&commands_dir).unwrap();
        commands_dir
    }

    /// Intercepting `--help` replaced clap's generated help, and what replaced
    /// it printed one line in a directory with no scripts -- so usage, the
    /// built-in commands and every flag the README documents were discoverable
    /// from no help output at all.
    #[test]
    fn help_always_shows_usage_and_the_builtins() -> Result<(), Box<dyn std::error::Error>> {
        let empty = tempdir()?;
        let home = tempdir()?;
        let _guard = crate::commands::ctx::testenv::EnvGuard::set(home.path(), Some(empty.path()));

        let mut out = Cursor::new(Vec::new());
        show_help(&mut out, false)?;
        let text = String::from_utf8(out.into_inner())?;

        for expected in [
            "Usage:",
            "create",
            "version",
            "init",
            "report",
            "ctx",
            "--dry-run",
            "--name",
            "--global",
        ] {
            assert!(
                text.contains(expected),
                "help must mention {expected}, got:\n{text}"
            );
        }
        Ok(())
    }

    /// Test that a local script file is listed correctly.
    #[test]
    fn test_show_help_with_script() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);

        // Create a dummy script file (YAML) in .zirv/commands.
        let script_content = r#"
name: "Test Script"
description: "A dummy script for testing."
params: []
commands: []
        "#;
        let script_file = commands_dir.join("test.yaml");
        write(&script_file, script_content)?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;

        let mut buffer = Cursor::new(Vec::new());
        show_help(&mut buffer, false)?;
        let output = String::from_utf8(buffer.into_inner())?;

        assert!(output.contains("File:"), "Output should contain 'File:'");

        assert!(
            output.contains("Test Script"),
            "Output should contain the script name 'Test Script'"
        );

        assert!(
            output.contains("Description:"),
            "Output should contain 'Description:'"
        );

        Ok(())
    }

    /// Test that shortcuts are listed in the help output.
    #[test]
    fn test_show_help_with_shortcuts() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);

        // Create a dummy script file.
        let script_content = r#"
name: "Test Script"
description: "A dummy script for testing shortcuts."
params: []
commands: []
        "#;
        let script_file = commands_dir.join("test.yaml");
        write(&script_file, script_content)?;

        // Create a shortcuts file (still at the .zirv root) mapping "t" to "test.yaml".
        let shortcuts_content = r#"
shortcuts:
  t: "test.yaml"
        "#;
        let shortcuts_file = zirv_dir.join(".shortcuts.yaml");
        write(&shortcuts_file, shortcuts_content)?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;

        let mut buffer = Cursor::new(Vec::new());
        show_help(&mut buffer, false)?;
        let output = String::from_utf8(buffer.into_inner())?;

        assert!(
            output.contains("Available Shortcuts:"),
            "Output should list shortcuts"
        );

        assert!(
            output.contains("t -> test.yaml"),
            "Output should contain the shortcut mapping 't -> test.yaml'"
        );

        assert!(
            output.contains("help, h"),
            "Output should include the built-in commands"
        );

        Ok(())
    }

    /// `zirv ctx` is a built-in, so it belongs in the command list next to
    /// init, create, version and help.
    #[test]
    fn test_show_help_lists_the_ctx_builtin() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);
        write(
            commands_dir.join("test.yaml"),
            "name: \"Test\"\ncommands: []\n",
        )?;
        write(
            zirv_dir.join(".shortcuts.yaml"),
            "shortcuts:\n  t: \"test.yaml\"\n",
        )?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;
        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer, false);
        result?;

        let output = String::from_utf8(buffer.into_inner())?;
        assert!(
            output.contains("ctx") && output.contains("Context management"),
            "got {output}"
        );
        Ok(())
    }

    /// `zirv chat`/`zirv agent` are top-level aliases for `zirv ctx chat`/
    /// `zirv ctx agent` (see `main.rs`'s `top_level_ctx_alias`); they belong
    /// in the help listing next to the other built-ins so they're
    /// discoverable, not just documented.
    #[test]
    fn help_lists_chat_and_agent_as_built_ins() -> Result<(), Box<dyn std::error::Error>> {
        let empty = tempdir()?;
        let home = tempdir()?;
        let _guard = crate::commands::ctx::testenv::EnvGuard::set(home.path(), Some(empty.path()));

        let mut out = Cursor::new(Vec::new());
        show_help(&mut out, false)?;
        let text = String::from_utf8(out.into_inner())?;

        assert!(text.contains("chat"), "got {text}");
        assert!(text.contains("agent"), "got {text}");
        Ok(())
    }

    /// `zirv memory` is a top-level built-in with its own verb tree
    /// (`is_top_level_memory` in `main.rs`); it belongs in the help listing
    /// next to the other built-ins so it's discoverable, not just documented.
    #[test]
    fn help_lists_memory_as_a_built_in() -> Result<(), Box<dyn std::error::Error>> {
        let empty = tempdir()?;
        let home = tempdir()?;
        let _guard = crate::commands::ctx::testenv::EnvGuard::set(home.path(), Some(empty.path()));

        let mut out = Cursor::new(Vec::new());
        show_help(&mut out, false)?;
        let text = String::from_utf8(out.into_inner())?;

        assert!(text.contains("memory"), "got {text}");
        Ok(())
    }

    /// `.zirv/ctx.toml` is a ctx config file, not a script. Parsing it as a
    /// Script used to make `zirv help` fail for the whole directory.
    #[test]
    fn test_show_help_ignores_ctx_config() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);

        write(
            commands_dir.join("test.yaml"),
            "name: \"Test Script\"\ncommands: []\n",
        )?;
        write(zirv_dir.join("ctx.toml"), "[score]\nwindow = 4\n")?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;

        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer, false);

        result?;
        let output = String::from_utf8(buffer.into_inner())?;
        assert!(output.contains("Test Script"));
        assert!(!output.contains("ctx.toml"));

        Ok(())
    }

    /// `.zirv/.settings.toml` is zirv's own agent switchboard, not a script.
    /// Mirrors `test_show_help_ignores_ctx_config` above.
    #[test]
    fn the_settings_file_is_not_listed_as_a_script() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);

        write(
            commands_dir.join("test.yaml"),
            "name: \"Test Script\"\ncommands: []\n",
        )?;
        write(
            zirv_dir.join(".settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;

        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer, false);

        result?;
        let output = String::from_utf8(buffer.into_inner())?;
        assert!(output.contains("Test Script"));
        assert!(!output.contains(".settings.toml"));

        Ok(())
    }

    /// Review finding 2: a differently-cased settings/ctx-config file
    /// (`.Settings.toml`, honored the same as `.settings.toml` by NTFS) must
    /// still be skipped, not parsed as a script and hard-error `zirv help`.
    #[test]
    fn a_differently_cased_settings_file_is_not_listed_as_a_script()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);

        write(
            commands_dir.join("test.yaml"),
            "name: \"Test Script\"\ncommands: []\n",
        )?;
        write(
            zirv_dir.join(".Settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;

        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer, false);

        result?;
        let output = String::from_utf8(buffer.into_inner())?;
        assert!(output.contains("Test Script"));
        assert!(!output.contains(".Settings.toml"));

        Ok(())
    }

    /// A script file named after a reserved command (e.g. `help.yaml`) is
    /// shadowed by the built-in of the same name and can never be invoked
    /// (`main.rs` matches built-ins before ever looking at `.zirv/`). The
    /// listing must flag it so the user isn't left wondering why it never runs.
    #[test]
    fn test_show_help_marks_reserved_script_name_as_shadowed()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);

        write(
            commands_dir.join("help.yaml"),
            "name: \"My Help Script\"\ncommands: []\n",
        )?;
        write(
            commands_dir.join("build.yaml"),
            "name: \"Build\"\ncommands: []\n",
        )?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;
        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer, false);
        result?;

        let output = String::from_utf8(buffer.into_inner())?;
        let help_line = output
            .lines()
            .find(|l| l.starts_with("File: help.yaml"))
            .unwrap_or("");
        assert!(
            help_line.contains("shadowed") || help_line.contains("unreachable"),
            "expected 'help.yaml' to be marked unreachable, got: {help_line}"
        );

        let build_line = output
            .lines()
            .find(|l| l.starts_with("File: build.yaml"))
            .unwrap_or("");
        assert!(
            !build_line.contains("shadowed"),
            "an ordinary script must not be marked shadowed, got: {build_line}"
        );

        Ok(())
    }

    /// S4: NTFS (and APFS by default) resolve a file case-insensitively, so
    /// `Chat.yaml` is exactly as unreachable as `chat.yaml` would be, even
    /// though `zirv Chat` is not literally intercepted as the `chat` alias
    /// (only the lowercase spelling is). The shadow marker has to catch a
    /// differently-cased collision, or a user creating one gets no warning
    /// anywhere it's listed.
    #[test]
    fn a_differently_cased_reserved_command_is_flagged_as_shadowed()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);

        write(
            commands_dir.join("Chat.yaml"),
            "name: \"My Chat Script\"\ncommands: []\n",
        )?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;
        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer, false);
        result?;

        let output = String::from_utf8(buffer.into_inner())?;
        let chat_line = output
            .lines()
            .find(|l| l.starts_with("File: Chat.yaml"))
            .unwrap_or("");
        assert!(
            chat_line.contains("shadowed") || chat_line.contains("unreachable"),
            "expected 'Chat.yaml' to be marked unreachable, got: {chat_line}"
        );
        Ok(())
    }

    /// Same idea for shortcuts: a `.shortcuts.yaml` entry keyed on a reserved
    /// letter (e.g. `c`, already `create`'s alias) can never be reached.
    #[test]
    fn test_show_help_marks_reserved_shortcut_as_shadowed() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        let commands_dir = setup_commands_dir(&zirv_dir);

        write(
            commands_dir.join("commit.yaml"),
            "name: \"Commit\"\ncommands: []\n",
        )?;
        write(
            zirv_dir.join(".shortcuts.yaml"),
            "shortcuts:\n  c: \"commit.yaml\"\n  gc: \"commit.yaml\"\n",
        )?;

        // NEW-1: a guard, so a failing assertion below cannot leave the whole
        // process sitting in a temp directory that is about to be deleted.
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(&temp_path)?;
        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer, false);
        result?;

        let output = String::from_utf8(buffer.into_inner())?;
        let c_line = output
            .lines()
            .find(|l| l.trim_start().starts_with("c ->"))
            .unwrap_or("");
        assert!(
            c_line.contains("shadowed") || c_line.contains("unreachable"),
            "expected the 'c' shortcut to be marked unreachable, got: {c_line}"
        );

        let gc_line = output
            .lines()
            .find(|l| l.trim_start().starts_with("gc ->"))
            .unwrap_or("");
        assert!(
            !gc_line.contains("shadowed"),
            "an ordinary shortcut must not be marked shadowed, got: {gc_line}"
        );

        Ok(())
    }
}
