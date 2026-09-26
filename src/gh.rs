//! Shell layer over the `gh` CLI.
//!
//! `gh` owns all GitHub authentication (tokens, hosts, enterprise configuration); this module
//! never handles credentials. Every call resolves the repository through `GIT_DIR`, so it works
//! from any jj workspace, and every PR lookup is a single GraphQL call.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU64;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::jj;
use crate::types::{Bookmark, CommitId, Owner, Repo};

/// The GraphQL fields requested for each pull request.
///
/// Deliberately narrower than a review-oriented tool needs: this tool decides *cleanup* from PR
/// state and head branch, so review decisions and check rollups are not requested.
const PR_NODE_FIELDS: &str = "number headRefName headRefOid baseRefName state isDraft url title headRepositoryOwner { login } mergeCommit { oid }";

/// A GitHub pull request number. Zero is not a valid PR number, so the type cannot hold one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrNum(NonZeroU64);

impl PrNum {
    /// Wraps a PR number, returning `None` for zero.
    pub fn new(number: u64) -> Option<Self> {
        NonZeroU64::new(number).map(Self)
    }

    /// The number itself.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for PrNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A pull request's state, as GitHub reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrState {
    /// Merged into its base branch.
    Merged,
    /// Closed without merging.
    Closed,
    /// Still open.
    Open,
}

/// A pull request as this tool reasons about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhPr {
    /// The PR number.
    pub number: PrNum,
    /// The head branch name, i.e. the bookmark the PR was opened from.
    pub head_ref_name: Bookmark,
    /// The head commit GitHub recorded for the PR, even if its branch was deleted.
    #[serde(default)]
    pub head_ref_oid: Option<CommitId>,
    /// The base branch name.
    pub base_ref_name: Bookmark,
    /// The PR state.
    pub state: PrState,
    /// Whether the PR is a draft.
    pub is_draft: bool,
    /// The PR's web URL.
    pub url: String,
    /// The PR title.
    pub title: String,
    /// The merge/squash commit on the base branch, when the PR merged.
    pub merge_commit_oid: Option<CommitId>,
    /// The owner of the PR's head repository, when it differs or is known.
    pub head_repo_owner: Option<Owner>,
    /// The owner of the repository the PR lives in.
    pub repo_owner: Owner,
    /// The name of the repository the PR lives in.
    pub repo: Repo,
}

impl GhPr {
    /// A one-line description for the plan output.
    pub fn summary(&self) -> String {
        format!("{} {} ({})", self.number, self.title, self.state_label())
    }

    /// The PR state in lower case, for prose.
    pub fn state_label(&self) -> &'static str {
        match self.state {
            PrState::Merged => "merged",
            PrState::Closed => "closed",
            PrState::Open => "open",
        }
    }
}

/// The repository `gh` resolves for this checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOverview {
    /// Owner of the repository hosting the pull requests: the parent repository for a fork.
    pub owner: Owner,
    /// Name of that repository.
    pub repo: Repo,
}

#[derive(Deserialize)]
struct OverviewJson {
    owner: Owner,
    repo: Repo,
}

/// Runs `gh` with `GIT_DIR` pointing at the repository's backing git store, so the command works
/// from any workspace and cannot resolve some other repository.
fn gh_command() -> Result<Command> {
    let mut cmd = Command::new("gh");
    cmd.env("GIT_DIR", jj::git_root()?);
    Ok(cmd)
}

/// Resolves the GitHub repository backing this checkout.
///
/// For a fork, this is the parent repository, because that is where the pull requests are.
pub fn repo_overview() -> Result<RepoOverview> {
    let output = gh_command()?
        .args([
            "repo",
            "view",
            "--json",
            "owner,name,parent",
            "-q",
            "{owner: (.parent.owner.login // .owner.login), repo: (.parent.name // .name)}",
        ])
        .output()
        .context("Failed to run `gh repo view`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh repo view failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("gh repo view output not UTF-8")?;
    let overview: OverviewJson =
        serde_json::from_str(stdout.trim()).context("Failed to parse `gh repo view` output")?;
    Ok(RepoOverview { owner: overview.owner, repo: overview.repo })
}

/// Builds the GraphQL query that looks up the given PR numbers and head branches.
///
/// A PR is discovered either by the `PR: #N` trailer on a commit, or by the name of the bookmark
/// it was opened from — the two can disagree (a stale trailer, or a rebuilt branch), so both are
/// queried and merged.
pub fn build_query(pr_nums: &[PrNum], head_branches: &[Bookmark]) -> String {
    use std::fmt::Write;

    let mut fields = String::new();
    for number in pr_nums {
        let _ =
            write!(fields, " pr{0}: pullRequest(number: {0}) {{ {PR_NODE_FIELDS} }}", number.get());
    }
    for (i, branch) in head_branches.iter().enumerate() {
        let escaped = serde_json::Value::String(branch.as_str().to_owned()).to_string();
        // A branch can be reused across PRs. One result in each state is enough to establish
        // whether it has an open PR and whether it has a PR eligible for either state filter.
        for (alias, state) in [("op", "OPEN"), ("mg", "MERGED"), ("cl", "CLOSED")] {
            let _ = write!(
                fields,
                " {alias}{i}: pullRequests(first: 1, headRefName: {escaped}, states: [{state}]) {{ nodes {{ {PR_NODE_FIELDS} }} }}"
            );
        }
    }

    format!(
        "query($owner: String!, $repo: String!) {{ repository(owner: $owner, name: $repo) {{ defaultBranchRef {{ name }}{fields} }} }}"
    )
}

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
    #[serde(default)]
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct GraphQlData {
    repository: Option<RepositoryData>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryData {
    default_branch_ref: Option<DefaultBranchRef>,
    #[serde(flatten, deserialize_with = "deserialize_pr_nodes")]
    pr_nodes: Vec<PrNode>,
}

#[derive(Deserialize)]
struct DefaultBranchRef {
    name: Bookmark,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: PrNum,
    head_ref_name: Bookmark,
    head_ref_oid: CommitId,
    base_ref_name: Bookmark,
    state: PrState,
    is_draft: bool,
    url: String,
    title: String,
    merge_commit: Option<MergeCommit>,
    head_repository_owner: Option<HeadRepoOwner>,
}

#[derive(Deserialize)]
struct MergeCommit {
    oid: CommitId,
}

#[derive(Deserialize)]
struct HeadRepoOwner {
    login: Owner,
}

/// Collects `PrNode`s out of the flattened alias map.
///
/// GitHub returns `prN: PrNode | null` for number lookups and connections for branch lookups,
/// including the separate open-PR lookup. Both shapes arrive in the same flattened object.
fn deserialize_pr_nodes<'de, D>(deserializer: D) -> std::result::Result<Vec<PrNode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let fields: BTreeMap<String, serde_json::Value> = BTreeMap::deserialize(deserializer)?;
    let mut nodes = Vec::new();
    for (key, value) in fields {
        if value.is_null() {
            continue;
        }
        if key.starts_with("pr") {
            let node = serde_json::from_value::<PrNode>(value)
                .map_err(|e| D::Error::custom(format!("failed to deserialize `{key}`: {e}")))?;
            nodes.push(node);
        } else if ["op", "mg", "cl"].iter().any(|prefix| key.starts_with(prefix)) {
            #[derive(Deserialize)]
            struct Connection {
                nodes: Vec<PrNode>,
            }
            let connection = serde_json::from_value::<Connection>(value)
                .map_err(|e| D::Error::custom(format!("failed to deserialize `{key}`: {e}")))?;
            nodes.extend(connection.nodes);
        }
    }
    Ok(nodes)
}

/// Parses a `gh api graphql` response into PRs plus the repository's default branch.
pub fn parse_response(
    stdout: &str,
    owner: &Owner,
    repo: &Repo,
) -> Result<(Vec<GhPr>, Option<Bookmark>)> {
    let response: GraphQlResponse =
        serde_json::from_str(stdout).context("Failed to parse GraphQL response")?;
    // `Option::filter` rather than a `let` chain, which would raise the minimum toolchain to 1.88.
    if let Some(errors) = response.errors.filter(|errors| !errors.is_empty()) {
        let messages: Vec<&str> = errors.iter().map(|error| error.message.as_str()).collect();
        bail!("GraphQL errors: {}", messages.join("; "));
    }
    let data = response.data.context("GraphQL response missing `data`")?.repository.context(
        "GraphQL response missing `repository` (not found, or insufficient permissions)",
    )?;

    let default_branch = data.default_branch_ref.map(|reference| reference.name);

    let mut prs = BTreeMap::new();
    for node in data.pr_nodes {
        let pr = GhPr {
            number: node.number,
            head_ref_name: node.head_ref_name,
            head_ref_oid: Some(node.head_ref_oid),
            base_ref_name: node.base_ref_name,
            state: node.state,
            is_draft: node.is_draft,
            url: node.url,
            title: node.title,
            merge_commit_oid: node.merge_commit.map(|merge| merge.oid),
            head_repo_owner: node.head_repository_owner.map(|owner| owner.login),
            repo_owner: owner.clone(),
            repo: repo.clone(),
        };
        // A PR found by both number and branch arrives twice; the two copies are identical.
        prs.entry(pr.number).or_insert(pr);
    }
    Ok((prs.into_values().collect(), default_branch))
}

/// Looks up every given PR number and head branch in one repository.
pub fn load_prs(
    owner: &Owner,
    repo: &Repo,
    pr_nums: &[PrNum],
    head_branches: &[Bookmark],
) -> Result<(Vec<GhPr>, Option<Bookmark>)> {
    let query = build_query(pr_nums, head_branches);
    let output = gh_command()?
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("repo={repo}"),
        ])
        .output()
        .context("Failed to run `gh api graphql`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api graphql failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("gh output not UTF-8")?;
    parse_response(&stdout, owner, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &str = r#"{
      "data": { "repository": {
        "defaultBranchRef": { "name": "main" },
        "pr1": { "number": 1, "headRefName": "feat-a", "headRefOid": "aaa", "baseRefName": "main", "state": "MERGED",
                 "isDraft": false, "url": "https://github.com/o/r/pull/1", "title": "A",
                 "headRepositoryOwner": { "login": "o" }, "mergeCommit": { "oid": "aaa" } },
        "mg1": { "nodes": [
                 { "number": 1, "headRefName": "feat-a", "headRefOid": "aaa", "baseRefName": "main", "state": "MERGED",
                   "isDraft": false, "url": "https://github.com/o/r/pull/1", "title": "A",
                   "headRepositoryOwner": { "login": "o" }, "mergeCommit": { "oid": "aaa" } } ] },
        "op1": { "nodes": [
                 { "number": 2, "headRefName": "feat-a", "headRefOid": "bbb", "baseRefName": "main", "state": "OPEN",
                   "isDraft": true, "url": "https://github.com/o/r/pull/2", "title": "B",
                   "headRepositoryOwner": null, "mergeCommit": null } ] },
        "cl9": { "nodes": [] }
      } }
    }"#;

    #[test]
    fn parses_pr_nodes_from_both_alias_shapes_and_dedupes() {
        let (prs, default_branch) =
            parse_response(RESPONSE, &Owner::new("o"), &Repo::new("r")).unwrap();
        assert_eq!(default_branch, Some(Bookmark::new("main")));
        assert_eq!(prs.len(), 2, "PR #1 arrives twice and must be deduped: {prs:?}");
        let merged = &prs[0];
        assert_eq!(merged.number, PrNum::new(1).unwrap());
        assert_eq!(merged.state, PrState::Merged);
        assert_eq!(merged.head_ref_name, Bookmark::new("feat-a"));
        assert_eq!(merged.head_ref_oid, Some(CommitId::new("aaa")));
        assert_eq!(merged.merge_commit_oid, Some(CommitId::new("aaa")));
        assert_eq!(merged.repo_owner, Owner::new("o"));
        let open = &prs[1];
        assert_eq!(open.state, PrState::Open);
        assert!(open.is_draft);
        assert_eq!(open.merge_commit_oid, None);
        assert_eq!(open.head_repo_owner, None);
    }

    #[test]
    fn surfaces_graphql_errors() {
        let error = r#"{"data": null, "errors": [{"message": "boom"}]}"#;
        let message =
            parse_response(error, &Owner::new("o"), &Repo::new("r")).unwrap_err().to_string();
        assert!(message.contains("boom"), "{message}");
    }

    #[test]
    fn query_names_every_lookup_and_escapes_branches() {
        let query = build_query(&[PrNum::new(7).unwrap()], &[Bookmark::new("feat/\"a\n")]);
        assert!(query.contains("pr7: pullRequest(number: 7)"), "{query}");
        assert!(
            query.contains(
                r#"mg0: pullRequests(first: 1, headRefName: "feat/\"a\n", states: [MERGED])"#
            ),
            "{query}"
        );
        assert!(
            query.contains(
                r#"op0: pullRequests(first: 1, headRefName: "feat/\"a\n", states: [OPEN])"#
            ),
            "{query}"
        );
        assert!(query.contains("cl0: pullRequests(first: 1,"), "{query}");
        assert!(query.contains("defaultBranchRef { name }"), "{query}");
        assert!(query.contains("headRefOid"), "{query}");
    }

    #[test]
    fn pr_num_rejects_zero() {
        assert_eq!(PrNum::new(0), None);
        assert_eq!(PrNum::new(12).unwrap().to_string(), "#12");
    }
}
