use console::style;
use std::fmt::Display;

pub fn error(msg: impl Display) {
    eprintln!("{} {msg}", style("error:").red().bold());
}

pub fn warn(msg: impl Display) {
    eprintln!("{} {msg}", style("warning:").yellow().bold());
}

/// Progress a human reads, not output a caller parses. On stderr with the
/// rest of it, so a setup script can redirect one without losing the other.
pub fn note(msg: impl Display) {
    eprintln!("{msg}");
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
