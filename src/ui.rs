//! Terminal interaction.

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write as _};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::gh::{GhPr, PrNum};
use crate::plan::{self, Action, Plan};
use crate::style;

/// Shows a spinner for a blocking step on a terminal and a single status line elsewhere.
///
/// The spinner is stopped before an error reaches the caller, so error messages never overwrite
/// an active progress line.
pub fn progress<T>(
    label: &str,
    completed: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if !std::io::stderr().is_terminal() {
        anstream::eprintln!("{label}...");
        return operation();
    }

    let result = thread::scope(|scope| {
        let (stop, stopped) = mpsc::channel();
        let worker = scope.spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"].map(style::accent);
            for frame in frames.iter().cycle() {
                {
                    let mut stderr = std::io::stderr().lock();
                    let _ = write!(stderr, "\r\x1b[2K{frame} {label}...");
                    let _ = stderr.flush();
                }
                match stopped.recv_timeout(Duration::from_millis(90)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });
        let result = operation();
        let _ = stop.send(());
        let _ = worker.join();
        result
    });

    let mut stderr = std::io::stderr().lock();
    let _ = write!(stderr, "\r\x1b[2K");
    let _ = stderr.flush();
    drop(stderr);
    if result.is_ok() {
        anstream::eprintln!("{} {completed}", style::ok("✓"));
    } else {
        anstream::eprintln!("{} {label}", style::err("✗"));
    }
    result
}

/// Renders a plan with color and PR links on a terminal, or stable plain text when redirected.
pub fn render_plan(plan: &Plan, prs: &[GhPr]) -> String {
    if std::io::stderr().is_terminal() {
        render_terminal_plan(plan, prs)
    } else {
        plan::render(plan)
    }
}

fn render_terminal_plan(plan: &Plan, prs: &[GhPr]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\n{}", style::heading("Cleanup plan"));
    if plan.actions.is_empty() {
        let _ = writeln!(out, "  {}", style::dim("Nothing to clean."));
    } else {
        for action in &plan.actions {
            let _ = writeln!(out, "  {} {}", style::accent("◆"), plan::render_action(action));
            for number in action_prs(action) {
                if let Some(pr) = prs.iter().find(|pr| pr.number == *number) {
                    let _ = writeln!(
                        out,
                        "    {} {} {} {}",
                        style::dim("↳"),
                        style::accent(&pr.number.to_string()),
                        style::dim(pr.state_label()),
                        pr.url
                    );
                }
            }
        }
    }
    if !plan.skipped.is_empty() {
        let _ = writeln!(out, "\n{}", style::heading("Skipped"));
        for skip in &plan.skipped {
            let _ = writeln!(out, "  {} {}: {}", style::warn("•"), skip.subject, skip.reason);
        }
    }
    if !plan.warnings.is_empty() {
        let _ = writeln!(out, "\n{}", style::heading("Warnings"));
        for warning in &plan.warnings {
            let _ = writeln!(out, "  {} {warning}", style::warn("!"));
        }
    }
    out
}

fn action_prs(action: &Action) -> &[PrNum] {
    match action {
        Action::AbandonBookmark { prs, .. } | Action::DeleteBookmark { prs, .. } => prs,
        Action::ForgetWorkspace { .. } | Action::RemoveWorkspace { .. } => &[],
    }
}

/// Asks the user to confirm. An empty answer means yes, matching the `[Y/n]` prompt.
///
/// Without a terminal there is no one to ask, so the answer is no unless `assume_yes` was
/// requested.
pub fn confirm(message: &str, assume_yes: bool) -> bool {
    if assume_yes {
        anstream::eprintln!("{message}");
        return true;
    }
    if !std::io::stdin().is_terminal() {
        anstream::eprintln!("{message} (not a terminal, pass --yes to confirm)");
        return false;
    }
    anstream::eprint!("{message} [Y/n] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    match std::io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => {
            let answer = answer.trim().to_ascii_lowercase();
            answer.is_empty() || matches!(answer.as_str(), "y" | "yes")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gh::PrState;
    use crate::plan::Skipped;
    use crate::types::{Bookmark, Owner, Repo};

    #[test]
    fn terminal_plan_shows_pr_link_and_protection_reason() {
        let number = PrNum::new(65).expect("nonzero PR number");
        let pr = GhPr {
            number,
            head_ref_name: Bookmark::new("feat-x"),
            head_ref_oid: None,
            base_ref_name: Bookmark::new("main"),
            state: PrState::Merged,
            is_draft: false,
            url: "https://github.com/acme/widgets/pull/65".to_owned(),
            title: "Feature".to_owned(),
            merge_commit_oid: None,
            head_repo_owner: None,
            repo_owner: Owner::new("acme"),
            repo: Repo::new("widgets"),
        };
        let plan = Plan {
            actions: vec![Action::DeleteBookmark {
                bookmark: Bookmark::new("feat-x"),
                prs: vec![number],
            }],
            skipped: vec![Skipped {
                subject: "bookmark \"another\"".to_owned(),
                reason: "checked out in a workspace".to_owned(),
            }],
            warnings: Vec::new(),
        };

        let output = render_terminal_plan(&plan, &[pr]);

        assert!(output.contains("Cleanup plan"));
        assert!(output.contains("https://github.com/acme/widgets/pull/65"));
        assert!(output.contains("Skipped"));
        assert!(output.contains("bookmark \"another\": checked out in a workspace"));
    }
}
