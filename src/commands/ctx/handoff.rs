// Handoff/parse_markdown/structural are consumed by the verb wiring added in
// Task A19; nothing calls them outside tests yet, so dead_code is silenced
// module-wide until then, matching config.rs/state.rs/log.rs/event.rs.
#![allow(dead_code)]

use std::io::Write;

use super::CtxResult;
use super::event::StructuralContext;

// TODO(A19): this placeholder Args/run pair is replaced by the real verb
// wiring once `store`, `latest_for_repo` and `run_with` land; kept here so the
// crate keeps compiling between A17/A18 and A19.
#[derive(Debug, clap::Args)]
pub struct HandoffArgs {
    #[arg(num_args = 0.., allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

pub fn run<W: Write>(_args: &HandoffArgs, _w: &mut W) -> CtxResult<i32> {
    Err("zirv ctx handoff is not implemented yet".into())
}

pub const SECTIONS: [&str; 6] = [
    "Task",
    "Done",
    "Remaining",
    "Next step",
    "Files touched",
    "Gotchas learned",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Handoff {
    pub task: String,
    pub done: Vec<String>,
    pub remaining: Vec<String>,
    pub next_step: String,
    pub files_touched: Vec<String>,
    pub gotchas: Vec<String>,
}

fn write_list(out: &mut String, heading: &str, items: &[String]) {
    out.push_str(&format!("## {heading}\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
    out.push('\n');
}

impl Handoff {
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("## Task\n{}\n\n", self.task));
        write_list(&mut out, "Done", &self.done);
        write_list(&mut out, "Remaining", &self.remaining);
        out.push_str(&format!("## Next step\n{}\n\n", self.next_step));
        write_list(&mut out, "Files touched", &self.files_touched);
        write_list(&mut out, "Gotchas learned", &self.gotchas);
        out
    }

    /// A handoff without a task or a next step is not worth restarting on.
    pub fn is_usable(&self) -> bool {
        !self.task.trim().is_empty() && !self.next_step.trim().is_empty()
    }
}

fn strip_bullet(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    // Numbered lists: "1. item"
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && trimmed[digits.len()..].starts_with(". ") {
        return Some(trimmed[digits.len() + 2..].trim().to_string());
    }
    None
}

pub fn parse_markdown(md: &str) -> Handoff {
    let mut handoff = Handoff::default();
    let mut section: Option<&str> = None;

    for line in md.lines() {
        if let Some(rest) = line.trim().strip_prefix("## ") {
            let name = rest.trim();
            section = SECTIONS
                .iter()
                .find(|s| s.eq_ignore_ascii_case(name))
                .copied();
            continue;
        }
        let Some(current) = section else { continue };
        let bullet = strip_bullet(line);
        let plain = line.trim();

        match current {
            "Task" => {
                if handoff.task.is_empty() && !plain.is_empty() {
                    handoff.task = bullet.unwrap_or_else(|| plain.to_string());
                }
            }
            "Next step" => {
                if handoff.next_step.is_empty() && !plain.is_empty() {
                    handoff.next_step = bullet.unwrap_or_else(|| plain.to_string());
                }
            }
            "Done" => handoff.done.extend(bullet),
            "Remaining" => handoff.remaining.extend(bullet),
            "Files touched" => handoff.files_touched.extend(bullet),
            "Gotchas learned" => handoff.gotchas.extend(bullet),
            _ => {}
        }
    }
    handoff
}

/// Mechanical extraction used when the distiller is unavailable or unusable.
/// Never fails and never returns something unusable.
pub fn structural(ctx: &StructuralContext) -> Handoff {
    let task = ctx
        .user_messages
        .last()
        .map(|m| m.lines().next().unwrap_or(m).trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "Unknown task (no user prompt found in the transcript)".to_string());

    let done: Vec<String> = ctx
        .assistant_texts
        .iter()
        .map(|t| t.lines().next().unwrap_or(t).trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    let remaining: Vec<String> = ctx
        .tool_errors
        .iter()
        .map(|e| format!("Unresolved error: {}", e.lines().next().unwrap_or(e).trim()))
        .collect();

    Handoff {
        task,
        done,
        remaining,
        next_step: "Re-read the files listed below, then continue the task above from where the previous session stopped.".to_string(),
        files_touched: ctx.files_touched.clone(),
        gotchas: vec!["This handoff was extracted mechanically, so it may be incomplete.".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::event::StructuralContext;

    fn sample() -> Handoff {
        Handoff {
            task: "Wire the payments webhook".to_string(),
            done: vec![
                "Added the route".to_string(),
                "Wrote the parser".to_string(),
            ],
            remaining: vec!["Signature verification".to_string()],
            next_step: "Add a failing test for an invalid signature".to_string(),
            files_touched: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec!["The provider sends two events per charge".to_string()],
        }
    }

    #[test]
    fn markdown_uses_the_documented_section_order() {
        let md = sample().to_markdown();
        let positions: Vec<usize> = SECTIONS
            .iter()
            .map(|s| {
                md.find(&format!("## {s}"))
                    .unwrap_or_else(|| panic!("{s} missing"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "sections out of order in:\n{md}"
        );
    }

    #[test]
    fn markdown_round_trips() {
        let original = sample();
        assert_eq!(parse_markdown(&original.to_markdown()), original);
    }

    #[test]
    fn parsing_tolerates_extra_prose_and_missing_sections() {
        let md = "Here is the handoff you asked for.\n\n## Task\nShip the thing\n\n## Next step\nRun the tests\n";
        let parsed = parse_markdown(md);
        assert_eq!(parsed.task, "Ship the thing");
        assert_eq!(parsed.next_step, "Run the tests");
        assert!(parsed.done.is_empty());
        assert!(parsed.remaining.is_empty());
    }

    #[test]
    fn parsing_accepts_both_bullet_styles() {
        let md = "## Done\n- first\n* second\n1. third\n";
        assert_eq!(parse_markdown(md).done, vec!["first", "second", "third"]);
    }

    #[test]
    fn is_usable_requires_a_task_and_a_next_step() {
        assert!(sample().is_usable());
        assert!(!Handoff::default().is_usable());
        assert!(
            !Handoff {
                task: "something".to_string(),
                ..Handoff::default()
            }
            .is_usable(),
            "a handoff with no next step is not something to stand on"
        );
    }

    #[test]
    fn structural_fallback_uses_the_last_prompt_as_the_task() {
        let ctx = StructuralContext {
            user_messages: vec!["old request".to_string(), "fix the flaky test".to_string()],
            assistant_texts: vec!["[zirv] narrowed it to the timer".to_string()],
            files_touched: vec!["src/timer.rs".to_string()],
            tool_errors: vec!["assertion failed: expected 3".to_string()],
        };
        let handoff = structural(&ctx);
        assert_eq!(handoff.task, "fix the flaky test");
        assert_eq!(handoff.files_touched, vec!["src/timer.rs"]);
        assert!(handoff.done.iter().any(|d| d.contains("narrowed it")));
        assert!(
            handoff
                .remaining
                .iter()
                .any(|r| r.contains("assertion failed"))
        );
        assert!(!handoff.next_step.is_empty(), "always leave a next step");
        assert!(handoff.is_usable());
    }

    #[test]
    fn structural_fallback_survives_an_empty_context() {
        let handoff = structural(&StructuralContext::default());
        assert!(
            handoff.is_usable(),
            "a restart must always have something to stand on"
        );
        assert!(handoff.to_markdown().contains("## Task"));
    }

    #[test]
    fn structural_markdown_has_no_em_dashes() {
        let ctx = StructuralContext {
            user_messages: vec!["do it".to_string()],
            ..StructuralContext::default()
        };
        assert!(!structural(&ctx).to_markdown().contains('\u{2014}'));
    }
}
