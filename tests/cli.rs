//! End-to-end tests: a real jj repository, a stubbed `gh`.
//!
//! These cover what the unit tests cannot: that the jj templates actually parse, that the
//! revsets resolve, and that applying a plan leaves the repository in the intended state.
//!
//! The tests skip themselves when `jj` is not installed so that a plain `cargo test` still works
//! on a machine without it; CI installs jj and therefore exercises them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use tempfile::TempDir;

/// A stubbed `gh`.
///
/// It ignores the query and always answers with the same repository: PR #1 from `feat-x`,
/// #2 from `open-one` (open), #3 from `closed-one` (closed), #5 from `ws-branch`, #6 from
/// `lock-me`, #8 from `ws-dirty`. Tests create the local bookmarks they care about; extra pull
/// requests are ignored by the planner.
const STUB_GH: &str = r#"#!/bin/sh
case "$1 $2" in
"repo view")
  printf '%s' '{"owner":"acme","repo":"widgets"}'
  ;;
"api graphql")
  head_oid() {
    "$REAL_JJ" --ignore-working-copy log --no-graph -r "$1" -T 'commit_id' 2>/dev/null
  }
  cat <<EOF
  {"data":{"repository":{"defaultBranchRef":{"name":"main"},
    "pr1":{"number":1,"headRefName":"feat-x","headRefOid":"$(head_oid feat-x)","baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/1","title":"work on feat-x",
           "headRepositoryOwner":{"login":"acme"},"mergeCommit":{"oid":"0000000000000000000000000000000000000000"}},
    "mg0":{"nodes":[{"number":1,"headRefName":"feat-x","headRefOid":"$(head_oid feat-x)","baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/1","title":"work on feat-x",
           "headRepositoryOwner":{"login":"acme"},"mergeCommit":{"oid":"0000000000000000000000000000000000000000"}}]},
    "op1":{"nodes":[{"number":2,"headRefName":"open-one","headRefOid":"$(head_oid open-one)","baseRefName":"main","state":"OPEN","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/2","title":"work on open-one",
           "headRepositoryOwner":null,"mergeCommit":null}]},
    "cl2":{"nodes":[{"number":3,"headRefName":"closed-one","headRefOid":"$(head_oid closed-one)","baseRefName":"main","state":"CLOSED","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/3","title":"work on closed-one",
           "headRepositoryOwner":null,"mergeCommit":null}]},
    "mg3":{"nodes":[{"number":5,"headRefName":"ws-branch","headRefOid":"$(head_oid ws-branch)","baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/5","title":"work on ws-branch",
           "headRepositoryOwner":null,"mergeCommit":{"oid":"0000000000000000000000000000000000000000"}}]},
    "mg4":{"nodes":[{"number":6,"headRefName":"lock-me","headRefOid":"$(head_oid lock-me)","baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/6","title":"work on lock-me",
           "headRepositoryOwner":null,"mergeCommit":{"oid":"0000000000000000000000000000000000000000"}}]},
    "mg5":{"nodes":[{"number":8,"headRefName":"ws-dirty","headRefOid":"$(head_oid ws-dirty)","baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/8","title":"work on ws-dirty",
           "headRepositoryOwner":null,"mergeCommit":{"oid":"0000000000000000000000000000000000000000"}}]},
    "mg6":{"nodes":[{"number":11,"headRefName":"stack-a","headRefOid":"$(head_oid stack-a)","baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/11","title":"work on stack-a",
           "headRepositoryOwner":null,"mergeCommit":{"oid":"0000000000000000000000000000000000000000"}}]},
    "mg7":{"nodes":[{"number":12,"headRefName":"stack-b","headRefOid":"$(head_oid stack-b)","baseRefName":"main","state":"MERGED","isDraft":false,
           "url":"https://github.com/acme/widgets/pull/12","title":"work on stack-b",
           "headRepositoryOwner":null,"mergeCommit":{"oid":"0000000000000000000000000000000000000000"}}]}}}}
EOF
  ;;
*)
  echo "stub gh: unexpected invocation: $*" >&2
  exit 2
  ;;
esac
"#;

/// A scratch repository with an origin, a `jh` stub, and the `jj-cleanup` binary wired up.
struct Scenario {
    dir: TempDir,
    repo: PathBuf,
    bin: PathBuf,
    stub_dir: PathBuf,
    jj: PathBuf,
}

impl Scenario {
    /// Builds the scenario, or `None` when jj is unavailable.
    fn new() -> Option<Self> {
        let jj = which("jj")?;
        let dir = TempDir::new().expect("temp dir");
        let repo = dir.path().join("repo");
        let origin = dir.path().join("origin.git");
        let stub_dir = dir.path().join("stub");
        fs::create_dir_all(&repo).expect("repo dir");
        fs::create_dir_all(&stub_dir).expect("stub dir");
        write_executable(&stub_dir.join("gh"), STUB_GH);

        let scratch = dir.path().to_path_buf();
        let scenario =
            Self { repo, bin: assert_cmd::cargo::cargo_bin("jj-cleanup"), stub_dir, jj, dir };

        let origin = origin.to_string_lossy().into_owned();
        run(
            &which("git").expect("git"),
            &["init", "--bare", "-q", "-b", "main", &origin],
            &scratch,
            None,
        );
        scenario.jj(&["git", "init", "--colocate"], &scenario.repo);
        scenario.jj(&["git", "remote", "add", "origin", &origin], &scenario.repo);
        scenario.jj(&["describe", "-m", "trunk"], &scenario.repo);
        scenario.jj(&["bookmark", "create", "main", "-r", "@"], &scenario.repo);
        scenario.jj(&["git", "push", "-b", "main"], &scenario.repo);
        scenario.jj(&["git", "fetch"], &scenario.repo);
        Some(scenario)
    }

    /// Runs jj in `cwd`, with a hermetic identity, and returns stdout.
    fn jj(&self, args: &[&str], cwd: &Path) -> String {
        let mut full = vec![
            "--config",
            "user.name=jj-cleanup test",
            "--config",
            "user.email=test@example.com",
        ];
        full.extend_from_slice(args);
        let output = run(&self.jj, &full, cwd, Some(&self.dir.path().join("config")));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Creates a commit above trunk with a bookmark on it, and pushes the bookmark.
    fn branch(&self, name: &str, message: &str) {
        self.jj(&["new", "main", "-m", message], &self.repo);
        self.jj(&["bookmark", "create", name, "-r", "@"], &self.repo);
        self.jj(&["git", "push", "-b", name], &self.repo);
    }

    /// Leaves the primary workspace on a fresh commit, as a real session would.
    fn settle(&self) {
        self.jj(&["new", "main"], &self.repo);
    }

    /// Runs `jj-cleanup`.
    fn tool(&self, args: &[&str]) -> Output {
        let path =
            format!("{}:{}", self.stub_dir.display(), std::env::var("PATH").unwrap_or_default());
        Command::new(&self.bin)
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", path)
            .env("REAL_JJ", &self.jj)
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .output()
            .expect("run jj-cleanup")
    }

    /// The live local bookmarks, one `name` per line.
    ///
    /// A deleted bookmark lingers as a tombstone (`target: [null]`, no `remote`) so that a later
    /// push can delete it on the remote; it is not a live bookmark and is filtered out here. The
    /// remote-tracking entries beside it are filtered out too.
    fn local_bookmarks(&self) -> String {
        self.jj(&["bookmark", "list", "-T", "json(self) ++ \"\\n\""], &self.repo)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|entry| entry.get("remote").is_none())
            .filter(|entry| {
                entry["target"]
                    .as_array()
                    .is_some_and(|targets| targets.iter().any(|t| t.is_string()))
            })
            .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// How many operations the repository has recorded.
    fn operations(&self) -> usize {
        self.jj(&["op", "log", "--no-graph", "-T", "self.id().short() ++ \"\\n\""], &self.repo)
            .lines()
            .count()
    }

    /// The commits above trunk, one description per line.
    fn above_trunk(&self) -> String {
        self.jj(
            &["log", "--no-graph", "-r", "trunk()..", "-T", "description.first_line() ++ \"\\n\""],
            &self.repo,
        )
    }

    /// Everything trunk contains, one description per line.
    fn in_trunk(&self) -> String {
        self.jj(
            &["log", "--no-graph", "-r", "::trunk()", "-T", "description.first_line() ++ \"\\n\""],
            &self.repo,
        )
    }

    /// The workspace names.
    fn workspaces(&self) -> String {
        self.jj(&["workspace", "list", "-T", "name ++ \"\\n\""], &self.repo)
    }
}

/// Runs a command, asserting success, and returns its output.
fn run(program: &Path, args: &[&str], cwd: &Path, config_home: Option<&Path>) -> Output {
    let mut command = std::process::Command::new(program);
    command.args(args).current_dir(cwd).env("JJ_CONFIG", "/dev/null");
    if let Some(config_home) = config_home {
        command.env("XDG_CONFIG_HOME", config_home);
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", program.display()));
    assert!(
        output.status.success(),
        "{} {:?} failed: {}",
        program.display(),
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Writes a file and marks it executable.
fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Finds a program on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Skips a test when jj is missing, explaining why.
macro_rules! scenario {
    () => {
        match Scenario::new() {
            Some(scenario) => scenario,
            None => {
                eprintln!("skipping: jj is not installed");
                return;
            }
        }
    };
}

#[test]
fn list_prints_the_plan_and_changes_nothing() {
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");
    scenario.settle();

    let before = scenario.operations();
    let output = scenario.tool(&["list", "--no-fetch", "--no-workspaces"]);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(text.contains("abandon 1 revision for bookmark \"feat-x\""), "{text}");
    assert!(text.contains("nothing was changed"), "{text}");
    assert_eq!(scenario.operations(), before, "a dry run must not record an operation");
    assert!(scenario.local_bookmarks().contains("feat-x"), "the bookmark must survive");
}

#[test]
fn clean_abandons_the_merged_bookmark() {
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");
    scenario.settle();

    let output = scenario.tool(&["cleanup", "-y"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    assert!(!scenario.local_bookmarks().contains("feat-x"), "bookmark should be gone");
    assert!(!scenario.above_trunk().contains("feat-x work"), "revision should be abandoned");
}

#[test]
fn a_bookmark_protected_by_the_current_workspace_can_be_cleaned_later() {
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");

    let first = scenario.tool(&["cleanup", "--no-fetch", "--no-workspaces", "-y"]);
    let first_text = String::from_utf8_lossy(&first.stderr);
    assert!(first.status.success(), "{first_text}");
    assert!(first_text.contains("checked out in a workspace"), "{first_text}");
    assert!(scenario.local_bookmarks().contains("feat-x"), "the bookmark must remain for retry");

    scenario.settle();
    let second = scenario.tool(&["cleanup", "--no-fetch", "--no-workspaces", "-y"]);
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert!(!scenario.local_bookmarks().contains("feat-x"), "the bookmark should be removed");
    assert!(!scenario.above_trunk().contains("feat-x work"), "the revision should be abandoned");
}

#[test]
fn a_kept_bookmark_protects_its_revisions() {
    let scenario = scenario!();
    // `shared` keeps the base alive; only `feat-x`'s own commit may be abandoned.
    scenario.branch("shared", "shared base");
    scenario.jj(&["new", "-m", "feat-x work"], &scenario.repo);
    scenario.jj(&["bookmark", "create", "feat-x", "-r", "@"], &scenario.repo);
    scenario.jj(&["git", "push", "-b", "feat-x"], &scenario.repo);
    scenario.settle();

    let output = scenario.tool(&["cleanup", "-y"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let above = scenario.above_trunk();
    assert!(above.contains("shared base"), "the kept bookmark's revision must survive: {above}");
    assert!(!above.contains("feat-x work"), "the merged revision should be abandoned: {above}");
    assert!(scenario.local_bookmarks().contains("shared"), "the kept bookmark must remain");
}

#[test]
fn forget_keeps_the_workspace_directory() {
    let scenario = scenario!();
    let workspace = scenario.dir.path().join("ws2");
    scenario.jj(
        &["workspace", "add", "--name", "ws2", "-r", "main", &workspace.to_string_lossy()],
        &scenario.repo,
    );
    scenario.jj(&["describe", "-m", "ws2 scratch"], &workspace);
    scenario.jj(&["new", "-m", "ws2 work"], &workspace);
    scenario.jj(&["bookmark", "create", "ws-branch", "-r", "@"], &workspace);
    scenario.jj(&["git", "push", "-b", "ws-branch"], &scenario.repo);
    scenario.settle();

    let output = scenario.tool(&["cleanup", "-y"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    assert!(!scenario.workspaces().contains("ws2"), "the workspace should be forgotten");
    assert!(workspace.is_dir(), "forgetting must leave the directory in place");
}

#[test]
fn remove_deletes_the_workspace_directory() {
    let scenario = scenario!();
    let workspace = scenario.dir.path().join("ws2");
    scenario.jj(
        &["workspace", "add", "--name", "ws2", "-r", "main", &workspace.to_string_lossy()],
        &scenario.repo,
    );
    scenario.jj(&["describe", "-m", "ws2 scratch"], &workspace);
    scenario.jj(&["new", "-m", "ws2 work"], &workspace);
    scenario.jj(&["bookmark", "create", "ws-branch", "-r", "@"], &workspace);
    scenario.jj(&["git", "push", "-b", "ws-branch"], &scenario.repo);
    scenario.settle();

    let output = scenario.tool(&["cleanup", "-y", "--remove"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    assert!(!scenario.workspaces().contains("ws2"), "the workspace should be forgotten");
    assert!(!workspace.exists(), "the directory should be deleted");
}

#[test]
fn a_workspace_with_uncommitted_changes_is_refused() {
    let scenario = scenario!();
    let workspace = scenario.dir.path().join("ws3");
    scenario.jj(
        &["workspace", "add", "--name", "ws3", "-r", "main", &workspace.to_string_lossy()],
        &scenario.repo,
    );
    scenario.jj(&["describe", "-m", "ws3 scratch"], &workspace);
    scenario.jj(&["new", "-m", "ws3 work"], &workspace);
    scenario.jj(&["bookmark", "create", "ws-dirty", "-r", "@"], &workspace);
    scenario.jj(&["git", "push", "-b", "ws-dirty"], &scenario.repo);
    fs::write(workspace.join("in-progress.txt"), "work").expect("write file");
    scenario.settle();

    let output = scenario.tool(&["list", "--no-fetch"]);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{text}");
    assert!(!text.contains("forget workspace \"ws3\""), "{text}");

    let applied = scenario.tool(&["cleanup", "--no-fetch", "-y"]);
    assert!(applied.status.success(), "{}", String::from_utf8_lossy(&applied.stderr));
    assert!(scenario.workspaces().contains("ws3"), "a dirty workspace must be left alone");
    assert!(workspace.join("in-progress.txt").is_file(), "its files must be untouched");
    // jj may still rewrite the commit that workspace has checked out — its bookmark's pull request
    // merged — so the invariant is that the uncommitted work is still there and still visible as a
    // working-copy change.
    let status = scenario.jj(&["status"], &workspace);
    assert!(status.contains("in-progress.txt"), "the uncommitted change must survive: {status}");
}

#[test]
fn an_open_pull_request_is_refused_without_touching_anything() {
    let scenario = scenario!();
    scenario.branch("open-one", "open work");
    scenario.settle();

    let before = scenario.operations();
    let output = scenario.tool(&["cleanup", "--no-fetch", "--no-workspaces", "-y"]);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(text.contains("bookmark \"open-one\": #2 is still open"), "{text}");
    assert!(text.contains("Nothing to clean"), "{text}");
    assert_eq!(scenario.operations(), before, "a refusal must not record an operation");
    assert!(scenario.above_trunk().contains("open work"));
}

#[test]
fn a_merged_pr_with_a_different_head_cannot_clean_a_reused_bookmark() {
    let scenario = scenario!();
    scenario.branch("feat-x", "new work on reused branch");
    scenario.settle();
    write_executable(
        &scenario.stub_dir.join("gh"),
        &STUB_GH.replace("$(head_oid feat-x)", "0000000000000000000000000000000000000000"),
    );

    let output = scenario.tool(&["cleanup", "--no-fetch", "--no-workspaces", "-y"]);
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("Nothing to clean"), "{text}");
    assert!(scenario.local_bookmarks().contains("feat-x"));
    assert!(scenario.above_trunk().contains("new work on reused branch"));
}

#[test]
fn the_pr_repository_default_branch_is_authoritative() {
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");
    scenario.settle();
    let stub = STUB_GH.replace(
        "\"defaultBranchRef\":{\"name\":\"main\"}",
        "\"defaultBranchRef\":{\"name\":\"feat-x\"}",
    );
    write_executable(&scenario.stub_dir.join("gh"), &stub);

    let output = scenario.tool(&["list", "--no-fetch", "--no-workspaces"]);
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("bookmark \"feat-x\": is the default branch"), "{text}");
    assert!(text.contains("Nothing to clean"), "{text}");
}

#[test]
fn a_locked_bookmark_is_refused() {
    let scenario = scenario!();
    scenario.branch("lock-me", "locked work");
    scenario.settle();

    let output = scenario.tool(&["lock", "lock-me"]);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{text}");
    assert!(text.contains("lock-me"), "{text}");

    let before = scenario.operations();
    let listed = scenario.tool(&["list", "--no-fetch", "--no-workspaces"]);
    let text = String::from_utf8_lossy(&listed.stderr).into_owned();
    assert!(text.contains("bookmark \"lock-me\": locked"), "{text}");
    assert_eq!(scenario.operations(), before);

    // Unlocking makes it cleanable again.
    scenario.tool(&["unlock", "lock-me"]);
    let listed = scenario.tool(&["list", "--no-fetch", "--no-workspaces"]);
    let text = String::from_utf8_lossy(&listed.stderr).into_owned();
    assert!(text.contains("abandon 1 revision for bookmark \"lock-me\""), "{text}");
}

#[test]
fn a_config_read_error_cannot_discard_bookmark_locks() {
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");
    scenario.settle();
    write_executable(
        &scenario.stub_dir.join("jj"),
        "#!/bin/sh\nif [ \"$1 $2\" = \"config get\" ]; then echo 'permission denied' >&2; exit 2; fi\nexec \"$REAL_JJ\" \"$@\"\n",
    );

    let output = scenario.tool(&["cleanup", "--no-fetch", "--no-workspaces", "-y"]);
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a failed lock read must stop cleanup: {text}");
    assert!(text.contains("jj config get jj-cleanup.locked-bookmarks failed"), "{text}");
    assert!(scenario.local_bookmarks().contains("feat-x"));
}

#[test]
fn an_old_jj_is_refused_with_a_clear_message() {
    let scenario = scenario!();
    write_executable(
        &scenario.stub_dir.join("jj"),
        "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'jj 0.37.0'; exit 0; fi\nexit 1\n",
    );
    scenario.settle();

    let output = scenario.tool(&["list", "--no-fetch"]);
    assert!(!output.status.success(), "an old jj must be refused");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(text.contains("too old"), "{text}");
    assert!(text.contains("0.44.0"), "{text}");
}

#[test]
fn install_aliases_makes_jj_cleanup_run_the_tool() {
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");
    scenario.settle();

    let installed = scenario.tool(&["util", "install-aliases", "--repo"]);
    assert!(installed.status.success(), "{}", String::from_utf8_lossy(&installed.stderr));

    // `jj cleanup` reaches the binary through a jj alias, so the binary must be on PATH.
    let bin_dir = scenario.bin.parent().expect("binary directory").to_path_buf();
    let path = format!(
        "{}:{}:{}",
        bin_dir.display(),
        scenario.stub_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = std::process::Command::new("jj")
        .args(["cleanup", "list", "--no-fetch", "--no-workspaces"])
        .current_dir(&scenario.repo)
        .env("PATH", path)
        .env("REAL_JJ", &scenario.jj)
        .env("XDG_CONFIG_HOME", scenario.dir.path().join("config"))
        .output()
        .expect("run jj cleanup");

    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{text}");
    assert!(text.contains("abandon 1 revision for bookmark \"feat-x\""), "{text}");

    // `jj cl` is installed as well.
    let short = std::process::Command::new("jj")
        .args(["cl", "list", "--no-fetch", "--no-workspaces"])
        .current_dir(&scenario.repo)
        .env("REAL_JJ", &scenario.jj)
        .env("XDG_CONFIG_HOME", scenario.dir.path().join("config"))
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                bin_dir.display(),
                scenario.stub_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("run jj cl");
    assert!(short.status.success(), "{}", String::from_utf8_lossy(&short.stderr));
}

#[test]
fn options_work_without_a_subcommand() {
    // `jj cleanup` runs the binary with no subcommand at all, so this is the documented form.
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");
    scenario.settle();

    let before = scenario.operations();
    let output = scenario.tool(&["--dry-run", "--no-fetch", "--no-workspaces"]);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(output.status.success(), "{text}");
    assert!(text.contains("abandon 1 revision for bookmark \"feat-x\""), "{text}");
    assert_eq!(scenario.operations(), before, "a dry run must not record an operation");
}

#[test]
fn a_merged_stack_is_cleaned_completely() {
    // Two merged bookmarks on one another. Abandoning the parent first would make jj rebase the
    // child onto trunk and leave it behind, so the child has to go first.
    let scenario = scenario!();
    scenario.branch("stack-a", "A parent work");
    scenario.jj(&["new", "-m", "B child work"], &scenario.repo);
    scenario.jj(&["bookmark", "create", "stack-b", "-r", "@"], &scenario.repo);
    scenario.jj(&["git", "push", "-b", "stack-b"], &scenario.repo);
    scenario.settle();

    let output = scenario.tool(&["cleanup", "--no-fetch", "--no-workspaces", "-y"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let live = scenario.local_bookmarks();
    assert!(!live.contains("stack-a"), "the parent bookmark should be gone: {live}");
    assert!(!live.contains("stack-b"), "the child bookmark should be gone: {live}");
    let above = scenario.above_trunk();
    assert!(!above.contains("A parent work"), "the parent revision should be gone: {above}");
    assert!(!above.contains("B child work"), "the child revision should be gone: {above}");
}

#[test]
fn a_branch_whose_commits_landed_in_trunk_is_only_deleted() {
    // trunk advanced past the branch — a real squash or merge-commit merge — so the branch has no
    // revisions of its own left and nothing may be abandoned.
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");
    scenario.jj(&["new", "feat-x", "-m", "trunk advance"], &scenario.repo);
    scenario.jj(&["bookmark", "set", "main", "-r", "@"], &scenario.repo);
    scenario.jj(&["git", "push", "-b", "main"], &scenario.repo);
    scenario.jj(&["git", "fetch"], &scenario.repo);
    scenario.settle();

    let output = scenario.tool(&["cleanup", "--no-fetch", "--no-workspaces", "-y"]);
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("delete bookmark \"feat-x\""),
        "expected a delete, not an abandon: {text}"
    );
    assert!(!scenario.local_bookmarks().contains("feat-x"), "the bookmark should be gone");
    assert!(
        scenario.in_trunk().contains("feat-x work"),
        "the revision is part of trunk now and must survive: {}",
        scenario.in_trunk()
    );
}

#[test]
fn util_dump_round_trips_the_fixture_format() {
    let scenario = scenario!();
    scenario.branch("feat-x", "feat-x work");
    scenario.settle();

    let output = scenario.tool(&["util", "dump"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let dumped: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("dump must be JSON");
    assert_eq!(dumped["default_bookmark"], "main");
    assert!(dumped["jj_entries"].as_array().is_some_and(|entries| !entries.is_empty()));
    assert!(
        dumped["prs"]
            .as_array()
            .is_some_and(|prs| prs.iter().any(|pr| pr["headRefName"] == "feat-x"))
    );
}
