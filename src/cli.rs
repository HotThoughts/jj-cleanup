//! Command-line surface.
//!
//! `jj cleanup` is a jj alias for `jj util exec -- jj-cleanup`, so the common invocation reaches this
//! binary with no subcommand at all. Every planning option therefore lives on the top level and is
//! `global`, which is what makes both `jj cleanup --dry-run` and `jj cleanup list --dry-run` valid.

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::plan::StateFilter;

/// Which remotes are scanned for pull requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScanMode {
    /// The repository `gh` resolves for this checkout, which is the primary remote's repository.
    Quick,
    /// Every GitHub remote this checkout has.
    Deep,
}

/// Clean up the local revisions, bookmarks, and workspaces left behind by merged or closed
/// GitHub pull requests.
///
/// `jj cleanup` reaches this binary through a jj alias, so with no subcommand at all the default is
/// a full clean.
#[derive(Debug, Parser)]
#[command(
    name = "jj-cleanup",
    version,
    after_help = "Run `jj-cleanup util install-aliases` to use this as `jj cleanup`.\n\n\
                  Unlike `git clean`, this never removes files from a working copy. It cleans up \
                  bookmarks, revisions, and workspaces."
)]
pub struct Cli {
    /// Skip the confirmation prompt.
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    /// Planning options, accepted before or after the subcommand.
    #[command(flatten)]
    pub plan: PlanArgs,

    /// Print the plan and change nothing.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// What to do. With no subcommand, the plan is applied after confirmation.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// How the plan is built.
#[derive(Debug, Clone, Args)]
pub struct PlanArgs {
    /// Which pull request states make a bookmark cleanable.
    #[arg(long, value_enum, default_value_t = StateFilter::Merged, global = true)]
    pub state: StateFilter,

    /// How wide to look for pull requests.
    #[arg(long, value_enum, default_value_t = ScanMode::Quick, global = true)]
    pub scan: ScanMode,

    /// Do not fetch before planning.
    #[arg(long, global = true)]
    pub no_fetch: bool,

    /// Leave workspaces alone.
    #[arg(long, global = true)]
    pub no_workspaces: bool,

    /// Delete a cleaned workspace's directory instead of only forgetting it.
    ///
    /// Uncommitted tracked changes are snapshotted first, but ignored files in that directory are
    /// lost.
    #[arg(long, global = true)]
    pub remove: bool,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Plan the cleanup, then apply it after confirmation. The default.
    Cleanup,
    /// Print the plan and change nothing. Always a dry run.
    List,
    /// Never treat these bookmarks as cleanup candidates.
    Lock(LockArgs),
    /// Stop protecting these bookmarks.
    Unlock(LockArgs),
    /// Plumbing commands.
    Util(UtilArgs),
}

/// Options for `jj cleanup lock` and `jj cleanup unlock`.
#[derive(Debug, Clone, Args)]
pub struct LockArgs {
    /// Bookmark names.
    #[arg(required = true, value_name = "BOOKMARK")]
    pub bookmarks: Vec<String>,
}

/// Options for `jj cleanup util`.
#[derive(Debug, Clone, Args)]
pub struct UtilArgs {
    /// Which plumbing command.
    #[command(subcommand)]
    pub command: UtilCommand,
}

/// Plumbing commands.
#[derive(Debug, Clone, Subcommand)]
pub enum UtilCommand {
    /// Print everything gathered from jj and gh as JSON. This is the fixture format.
    Dump,
    /// Install the `clean` and `cl` jj aliases.
    InstallAliases(InstallAliasesArgs),
}

/// Options for `jj cleanup util install-aliases`.
#[derive(Debug, Clone, Args)]
pub struct InstallAliasesArgs {
    /// Install into this repository's config instead of the user's.
    #[arg(long)]
    pub repo: bool,
}
