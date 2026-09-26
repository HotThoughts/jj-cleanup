//! `jj-cleanup` — remove the local jj revisions, bookmarks, and workspaces left behind after a
//! GitHub pull request is merged or closed.
//!
//! The crate is a library so the planning rules in [`plan`] can be pinned from tests: planning is
//! a pure function of [`plan::InputData`], and [`run`] is the only part that talks to `jj` and
//! `gh`.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod apply;
pub mod cli;
pub mod gh;
pub mod jj;
pub mod lock;
pub mod plan;
pub mod style;
pub mod types;
pub mod ui;

use std::collections::{BTreeSet, HashMap};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cli::{
    Cli, Command as CliCommand, InstallAliasesArgs, LockArgs, PlanArgs, ScanMode, UtilArgs,
    UtilCommand,
};
use crate::gh::{GhPr, PrNum};
use crate::plan::{InputData, PlanOptions};
use crate::types::{Bookmark, Owner, Remote, Repo};

/// The gathered state, kept so a panic can dump what the run was looking at.
static INPUT: OnceLock<InputData> = OnceLock::new();

/// Runs the command line.
pub fn run() -> Result<()> {
    install_panic_hook();
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(CliCommand::Util(UtilArgs { command: UtilCommand::InstallAliases(args) })) => {
            return install_aliases(args);
        }
        Some(CliCommand::Lock(args)) => return change_locks(args, true),
        Some(CliCommand::Unlock(args)) => return change_locks(args, false),
        _ => {}
    }

    jj::version_check()?;

    // `list` is the same plan, never applied.
    let dry_run = cli.dry_run || matches!(cli.command, Some(CliCommand::List));
    let input = collect(&cli.plan)?;

    if let Some(CliCommand::Util(UtilArgs { command: UtilCommand::Dump })) = &cli.command {
        serde_json::to_writer(std::io::stdout(), &input)
            .context("Failed to write the state dump")?;
        println!();
        return Ok(());
    }

    let options = PlanOptions {
        state: cli.plan.state,
        workspaces: !cli.plan.no_workspaces,
        remove_workspaces: cli.plan.remove,
    };
    let plan = plan::build_plan(&input, &options);
    let rendered = ui::render_plan(&plan, &input.prs);
    let _ = INPUT.set(input);

    anstream::eprint!("{rendered}");

    if plan.is_empty() {
        return Ok(());
    }
    if dry_run {
        anstream::eprintln!("\n{}", style::warn("Dry run: nothing was changed."));
        return Ok(());
    }
    if !ui::confirm(&format!("Apply {} action(s)?", plan.actions.len()), cli.yes) {
        bail!("Aborted.");
    }
    ui::progress(
        &format!("Applying {} action(s)", plan.actions.len()),
        &format!("Applied {} action(s)", plan.actions.len()),
        || apply::execute(&plan),
    )
}

/// Gathers the state the planner needs.
fn collect(args: &PlanArgs) -> Result<InputData> {
    let remotes = ui::progress("Reading remotes", "Read remotes", jj::load_remotes)?;
    if !args.no_fetch {
        let fetched: Vec<Remote> = match args.scan {
            // jj's own default remote, which is what the primary repository is resolved from.
            ScanMode::Quick => Vec::new(),
            ScanMode::Deep => remotes.keys().cloned().collect(),
        };
        let described = match fetched.is_empty() {
            true => "the default remote".to_owned(),
            false => fetched.iter().map(Remote::to_string).collect::<Vec<_>>().join(", "),
        };
        ui::progress(
            &format!("Fetching from {described}"),
            &format!("Fetched from {described}"),
            || jj::git_fetch(&fetched),
        )?;
    }

    let jj_entries =
        ui::progress("Reading revisions and bookmarks", "Read revisions and bookmarks", || {
            jj::load_entries(jj::PLANNING_REVSET)
        })?;
    let workspaces = ui::progress("Checking workspaces", "Checked workspaces", || {
        jj::load_workspaces(!args.no_workspaces)
    })?;

    let pr_numbers: BTreeSet<PrNum> = jj_entries
        .iter()
        .filter_map(|entry| jj::parse_pr_trailer(&entry.commit.description))
        .collect();
    let local_bookmarks: BTreeSet<Bookmark> = jj_entries
        .iter()
        .flat_map(|entry| entry.local_bookmarks.iter().map(|bookmark| bookmark.name.clone()))
        .collect();

    let (prs, default_bookmark, load_warnings) =
        ui::progress("Fetching pull requests", "Fetched pull requests", || {
            load_prs(args.scan, &remotes, &pr_numbers, &local_bookmarks)
        })?;

    Ok(InputData {
        jj_entries,
        workspaces,
        prs,
        default_bookmark,
        primary_workspace_root: jj::primary_workspace_root(),
        locks: lock::load()?,
        load_warnings,
    })
}

/// Loads the pull requests for the configured scan scope.
///
/// A pull request number is only unique within one repository, so when a number turns up in more
/// than one scanned repository every pull request with that number is dropped. Dropping
/// associations can only ever remove cleanup candidates, never add one.
fn load_prs(
    scan: ScanMode,
    remotes: &std::collections::BTreeMap<Remote, (Owner, Repo)>,
    pr_numbers: &BTreeSet<PrNum>,
    local_bookmarks: &BTreeSet<Bookmark>,
) -> Result<(Vec<GhPr>, Option<Bookmark>, Vec<String>)> {
    let overview = gh::repo_overview()?;
    let mut targets = vec![(overview.owner, overview.repo)];
    if scan == ScanMode::Deep {
        for identity in remotes.values() {
            if !targets.contains(identity) {
                targets.push(identity.clone());
            }
        }
    }

    let numbers: Vec<PrNum> = pr_numbers.iter().copied().collect();
    let branches: Vec<Bookmark> = local_bookmarks.iter().cloned().collect();

    let mut warnings = Vec::new();
    let mut prs = Vec::new();
    let mut default_bookmark = None;
    for (index, (owner, repo)) in targets.iter().enumerate() {
        let (found, branch) = gh::load_prs(owner, repo, &numbers, &branches)?;
        if index == 0 {
            // The overview can describe a fork while this query targets its parent repository.
            // The parent's default branch is authoritative for the PRs being cleaned.
            default_bookmark = branch;
        }
        prs.extend(found);
    }

    // Detect numbers that are not unique across the scanned repositories.
    let mut seen: HashMap<PrNum, BTreeSet<String>> = HashMap::new();
    for pr in &prs {
        seen.entry(pr.number).or_default().insert(format!("{}/{}", pr.repo_owner, pr.repo));
    }
    let ambiguous: BTreeSet<PrNum> = seen
        .iter()
        .filter(|(_, repositories)| repositories.len() > 1)
        .map(|(number, _)| *number)
        .collect();
    if !ambiguous.is_empty() {
        let described: Vec<String> = ambiguous
            .iter()
            .map(|number| {
                let repositories: Vec<&str> = seen[number].iter().map(String::as_str).collect();
                format!("{number} (in {})", repositories.join(" and "))
            })
            .collect();
        warnings.push(format!(
            "Ignored the ambiguous pull request number(s) {}: a `PR: #N` trailer cannot say which repository it means.",
            described.join(", ")
        ));
        prs.retain(|pr| !ambiguous.contains(&pr.number));
    }

    Ok((prs, default_bookmark, warnings))
}

/// Adds or removes bookmark locks.
fn change_locks(args: &LockArgs, add: bool) -> Result<()> {
    let names: Vec<Bookmark> = args.bookmarks.iter().map(Bookmark::new).collect();
    let locks = match add {
        true => lock::lock(&names)?,
        false => lock::unlock(&names)?,
    };
    report_locks(&locks);
    Ok(())
}

/// Prints the current lock list.
fn report_locks(locks: &BTreeSet<Bookmark>) {
    if locks.is_empty() {
        anstream::eprintln!("No bookmarks are locked in this repository.");
        return;
    }
    anstream::eprintln!("Locked bookmarks (this repository):");
    for name in locks {
        anstream::eprintln!("  {name}");
    }
}

/// Writes the `clean` and `cl` jj aliases.
fn install_aliases(args: &InstallAliasesArgs) -> Result<()> {
    let scope = match args.repo {
        true => "--repo",
        false => "--user",
    };
    for name in ["cleanup", "cl"] {
        let output = Command::new("jj")
            .args([
                "config",
                "set",
                scope,
                &format!("aliases.{name}"),
                r#"["util", "exec", "--", "jj-cleanup"]"#,
            ])
            .output()
            .with_context(|| format!("Failed to run `jj config set` for aliases.{name}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("jj config set {scope} aliases.{name} failed: {}", stderr.trim());
        }
    }
    anstream::eprintln!("Installed into {scope} config:");
    anstream::eprintln!("  jj cleanup — plan and clean up merged or closed pull requests");
    anstream::eprintln!("  jj cl    — the same thing, shorter");
    Ok(())
}

/// Dumps the state the run was looking at when it panicked.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        match INPUT.get() {
            None => anstream::eprintln!(
                "jj-cleanup panicked before any state was gathered. Nothing to dump."
            ),
            Some(input) => match serde_json::to_string_pretty(input) {
                Ok(json) => {
                    anstream::eprintln!("jj-cleanup panicked. The gathered state was:\n{json}")
                }
                Err(error) => anstream::eprintln!(
                    "jj-cleanup panicked and its state could not be serialized: {error}"
                ),
            },
        }
        previous(info);
    }));
}
