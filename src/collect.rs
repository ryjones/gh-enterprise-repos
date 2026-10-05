use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde_json::json;

use crate::client::GithubClient;
use crate::model::*;

const ENTERPRISE_ORGS: &str = r#"
query($slug: String!, $cursor: String, $batchSize: Int!) {
  enterprise(slug: $slug) {
    organizations(first: $batchSize, after: $cursor, orderBy: {field: LOGIN, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes { login }
    }
  }
}
"#;

const ORG_REPOS: &str = r#"
query(
  $login: String!
  $cursor: String
  $batchSize: Int!
  $visibility: RepositoryVisibility
  $isArchived: Boolean
  $isFork: Boolean
  $withTopics: Boolean!
) {
  organization(login: $login) {
    repositories(
      first: $batchSize
      after: $cursor
      orderBy: {field: NAME, direction: ASC}
      ownerAffiliations: [OWNER]
      visibility: $visibility
      isArchived: $isArchived
      isFork: $isFork
    ) {
      pageInfo { hasNextPage endCursor }
      nodes {
        name
        nameWithOwner
        url
        description
        visibility
        isArchived
        isFork
        isTemplate
        isEmpty
        stargazerCount
        forkCount
        createdAt
        updatedAt
        pushedAt
        defaultBranchRef { name }
        primaryLanguage { name }
        licenseInfo { spdxId name }
        repositoryTopics(first: 20) @include(if: $withTopics) {
          nodes { topic { name } }
        }
      }
    }
  }
}
"#;

const ORG_TEAMS: &str = r#"
query($login: String!, $cursor: String, $batchSize: Int!) {
  organization(login: $login) {
    teams(first: $batchSize, after: $cursor, orderBy: {field: NAME, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        slug
        repositories(first: $batchSize) {
          pageInfo { hasNextPage endCursor }
          edges { permission node { name } }
        }
      }
    }
  }
}
"#;

const TEAM_REPOS: &str = r#"
query($login: String!, $slug: String!, $cursor: String, $batchSize: Int!) {
  organization(login: $login) {
    team(slug: $slug) {
      repositories(first: $batchSize, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        edges { permission node { name } }
      }
    }
  }
}
"#;

/// Which repositories to ask for. The same values are sent to GitHub as query
/// arguments and re-checked locally, so a server that ignores an argument
/// cannot widen the result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filter {
    pub visibility: Visibility,
    pub archived: Archived,
    pub forks: Forks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Visibility {
    #[default]
    Public,
    Private,
    Internal,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Archived {
    #[default]
    Exclude,
    Include,
    Only,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Forks {
    #[default]
    Include,
    Exclude,
    Only,
}

impl Filter {
    /// `RepositoryVisibility` argument; `None` means "do not filter".
    pub fn visibility_arg(&self) -> Option<&'static str> {
        match self.visibility {
            Visibility::Public => Some("PUBLIC"),
            Visibility::Private => Some("PRIVATE"),
            Visibility::Internal => Some("INTERNAL"),
            Visibility::All => None,
        }
    }

    pub fn is_archived_arg(&self) -> Option<bool> {
        match self.archived {
            Archived::Exclude => Some(false),
            Archived::Include => None,
            Archived::Only => Some(true),
        }
    }

    pub fn is_fork_arg(&self) -> Option<bool> {
        match self.forks {
            Forks::Include => None,
            Forks::Exclude => Some(false),
            Forks::Only => Some(true),
        }
    }

    /// Whether a repository GitHub returned really belongs in the output.
    ///
    /// A field the token could not read comes back null; that is treated as
    /// "not archived" / "not a fork", matching how GitHub itself defaults them,
    /// and an unknown visibility is left in rather than silently dropped.
    pub fn keep(&self, repo: &RepoNode) -> bool {
        if let (Some(expected), Some(actual)) = (self.visibility_arg(), repo.visibility.as_deref())
            && !actual.eq_ignore_ascii_case(expected)
        {
            return false;
        }
        if let Some(expected) = self.is_archived_arg()
            && repo.is_archived.unwrap_or(false) != expected
        {
            return false;
        }
        if let Some(expected) = self.is_fork_arg()
            && repo.is_fork.unwrap_or(false) != expected
        {
            return false;
        }
        true
    }

    pub fn describe(&self) -> Filters {
        let word = |v: &dyn std::fmt::Debug| format!("{v:?}").to_lowercase();
        Filters {
            visibility: word(&self.visibility),
            archived: word(&self.archived),
            forks: word(&self.forks),
        }
    }
}

/// Everything read out of a single organization.
pub struct OrgSnapshot {
    pub login: String,
    pub repositories: Vec<RepoNode>,
    /// Team access per repository; `None` when teams were not requested.
    pub teams: Option<TeamAccess>,
}

/// Lowercased repository name -> team slug -> permission (`admin`,
/// `maintain`, `write`, `triage` or `read`).
pub type TeamAccess = HashMap<String, BTreeMap<String, String>>;

pub struct Collector<'a> {
    client: &'a GithubClient,
    filter: Filter,
    with_topics: bool,
    with_teams: bool,
    batch_size: u32,
}

impl<'a> Collector<'a> {
    pub fn new(
        client: &'a GithubClient,
        filter: Filter,
        with_topics: bool,
        with_teams: bool,
        batch_size: u32,
    ) -> Self {
        Self {
            client,
            filter,
            with_topics,
            with_teams,
            batch_size: batch_size.clamp(1, 100),
        }
    }

    /// Confirm the endpoint and token work before spending a long run on them,
    /// and prime the client's view of the rate-limit budget.
    pub async fn viewer_login(&self) -> Result<String> {
        let data: ViewerData = self
            .client
            .query("query { viewer { login } }", json!({}))
            .await
            .context("could not authenticate to the GraphQL endpoint")?;
        Ok(data.viewer.login)
    }

    /// All organizations in an enterprise, ordered by login.
    pub async fn enterprise_orgs(&self, slug: &str) -> Result<Vec<String>> {
        let mut cursor: Option<String> = None;
        let mut out = Vec::new();

        loop {
            let data: EnterpriseOrgsData = self
                .client
                .query(
                    ENTERPRISE_ORGS,
                    json!({ "slug": slug, "cursor": cursor, "batchSize": self.batch_size }),
                )
                .await
                .with_context(|| format!("listing organizations in enterprise `{slug}`"))?;

            let enterprise = data.enterprise.with_context(|| {
                format!("no enterprise named `{slug}` is visible to this token")
            })?;

            out.extend(
                enterprise
                    .organizations
                    .nodes
                    .into_iter()
                    .flatten()
                    .map(|n| n.login),
            );

            let page = enterprise.organizations.page_info;
            if !page.has_next_page {
                break;
            }
            cursor = page.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(out)
    }

    /// Every repository in one organization that passes the filter.
    pub async fn org_snapshot(&self, login: &str) -> Result<OrgSnapshot> {
        let mut cursor: Option<String> = None;
        let mut out: Vec<RepoNode> = Vec::new();

        loop {
            let data: OrgReposData = self
                .client
                .query(
                    ORG_REPOS,
                    json!({
                        "login": login,
                        "cursor": cursor,
                        "batchSize": self.batch_size,
                        "visibility": self.filter.visibility_arg(),
                        "isArchived": self.filter.is_archived_arg(),
                        "isFork": self.filter.is_fork_arg(),
                        "withTopics": self.with_topics,
                    }),
                )
                .await
                .with_context(|| format!("listing repositories of `{login}`"))?;

            let org = data.organization.with_context(|| {
                format!("no organization named `{login}` is visible to this token")
            })?;

            let connection = org.repositories;
            out.extend(
                connection
                    .nodes
                    .into_iter()
                    .flatten()
                    .filter(|repo| self.filter.keep(repo)),
            );

            if !connection.page_info.has_next_page {
                break;
            }
            cursor = connection.page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        // Teams failing fails the organization: a snapshot without them would
        // make every repository look as if no team had access to it.
        let teams = if self.with_teams {
            Some(self.org_team_access(login).await?)
        } else {
            None
        };

        Ok(OrgSnapshot {
            login: login.to_string(),
            repositories: out,
            teams,
        })
    }

    /// Which teams reach which repositories in one organization.
    ///
    /// GraphQL has no teams field on a repository, so this walks the
    /// organization's teams and inverts each one's repository list. The first
    /// page of every team's repositories rides along with the team listing; a
    /// team with more than one page is followed up on its own.
    async fn org_team_access(&self, login: &str) -> Result<TeamAccess> {
        let mut access = TeamAccess::new();
        let mut cursor: Option<String> = None;

        loop {
            let data: OrgTeamsData = self
                .client
                .query(
                    ORG_TEAMS,
                    json!({ "login": login, "cursor": cursor, "batchSize": self.batch_size }),
                )
                .await
                .with_context(|| format!("listing teams of `{login}`"))?;

            let org = data.organization.with_context(|| {
                format!("no organization named `{login}` is visible to this token")
            })?;

            for team in org.teams.nodes.into_iter().flatten() {
                let mut page = team.repositories.page_info;
                record_team_repos(&mut access, &team.slug, team.repositories.edges);

                while page.has_next_page && page.end_cursor.is_some() {
                    let data: TeamReposData = self
                        .client
                        .query(
                            TEAM_REPOS,
                            json!({
                                "login": login,
                                "slug": team.slug,
                                "cursor": page.end_cursor,
                                "batchSize": self.batch_size,
                            }),
                        )
                        .await
                        .with_context(|| {
                            format!("listing repositories of team `{login}/{}`", team.slug)
                        })?;
                    let Some(repositories) = data
                        .organization
                        .and_then(|org| org.team)
                        .map(|team| team.repositories)
                    else {
                        break;
                    };
                    page = repositories.page_info;
                    record_team_repos(&mut access, &team.slug, repositories.edges);
                }
            }

            if !org.teams.page_info.has_next_page {
                break;
            }
            cursor = org.teams.page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(access)
    }
}

/// File one team's repositories under each repository's name. An edge whose
/// repository the token cannot read has no name to file it under.
fn record_team_repos(access: &mut TeamAccess, team: &str, edges: Vec<Option<TeamRepoEdge>>) {
    for edge in edges.into_iter().flatten() {
        let Some(repo) = edge.node else { continue };
        let permission = edge
            .permission
            .map(|p| p.to_lowercase())
            .unwrap_or_else(|| "unknown".to_string());
        access
            .entry(repo.name.to_lowercase())
            .or_default()
            .insert(team.to_string(), permission);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(visibility: Visibility, archived: Archived, forks: Forks) -> Filter {
        Filter {
            visibility,
            archived,
            forks,
        }
    }

    fn repo(visibility: &str, archived: bool, fork: bool) -> RepoNode {
        RepoNode {
            name: "example".into(),
            name_with_owner: None,
            url: None,
            description: None,
            visibility: Some(visibility.into()),
            is_archived: Some(archived),
            is_fork: Some(fork),
            is_template: None,
            is_empty: None,
            stargazer_count: None,
            fork_count: None,
            created_at: None,
            updated_at: None,
            pushed_at: None,
            default_branch_ref: None,
            primary_language: None,
            license_info: None,
            repository_topics: None,
        }
    }

    #[test]
    fn the_default_filter_is_public_and_unarchived() {
        let default = filter(Visibility::default(), Archived::default(), Forks::default());
        assert_eq!(default.visibility_arg(), Some("PUBLIC"));
        assert_eq!(default.is_archived_arg(), Some(false));
        assert_eq!(default.is_fork_arg(), None);

        assert!(default.keep(&repo("PUBLIC", false, false)));
        assert!(default.keep(&repo("PUBLIC", false, true)));
        assert!(!default.keep(&repo("PUBLIC", true, false)));
        assert!(!default.keep(&repo("PRIVATE", false, false)));
        assert!(!default.keep(&repo("INTERNAL", false, false)));
    }

    #[test]
    fn all_visibility_sends_no_argument_and_keeps_everything() {
        let any = filter(Visibility::All, Archived::Include, Forks::Include);
        assert_eq!(any.visibility_arg(), None);
        assert_eq!(any.is_archived_arg(), None);
        assert!(any.keep(&repo("PRIVATE", true, true)));
        assert!(any.keep(&repo("PUBLIC", false, false)));
    }

    #[test]
    fn only_variants_invert_the_filter() {
        let archived_only = filter(Visibility::All, Archived::Only, Forks::Include);
        assert_eq!(archived_only.is_archived_arg(), Some(true));
        assert!(archived_only.keep(&repo("PUBLIC", true, false)));
        assert!(!archived_only.keep(&repo("PUBLIC", false, false)));

        let forks_only = filter(Visibility::All, Archived::Include, Forks::Only);
        assert_eq!(forks_only.is_fork_arg(), Some(true));
        assert!(forks_only.keep(&repo("PUBLIC", false, true)));
        assert!(!forks_only.keep(&repo("PUBLIC", false, false)));

        let no_forks = filter(Visibility::All, Archived::Include, Forks::Exclude);
        assert_eq!(no_forks.is_fork_arg(), Some(false));
        assert!(!no_forks.keep(&repo("PUBLIC", false, true)));
    }

    #[test]
    fn unreadable_fields_do_not_drop_a_repository() {
        let default = filter(Visibility::Public, Archived::Exclude, Forks::Include);
        let mut sparse = repo("PUBLIC", false, false);
        sparse.visibility = None;
        sparse.is_archived = None;
        sparse.is_fork = None;
        // Unknown visibility is kept; a null `isArchived` reads as not archived.
        assert!(default.keep(&sparse));
    }

    fn edge(repo: Option<&str>, permission: Option<&str>) -> Option<TeamRepoEdge> {
        Some(TeamRepoEdge {
            permission: permission.map(str::to_string),
            node: repo.map(|name| NamedNode { name: name.into() }),
        })
    }

    #[test]
    fn team_repositories_are_inverted_into_per_repository_access() {
        let mut access = TeamAccess::new();
        record_team_repos(
            &mut access,
            "maintainers",
            vec![
                edge(Some("Widget-Kit"), Some("MAINTAIN")),
                edge(Some("docs"), Some("WRITE")),
                // A repository the token cannot read, and a dropped edge.
                edge(None, Some("ADMIN")),
                None,
            ],
        );
        record_team_repos(
            &mut access,
            "admins",
            vec![edge(Some("widget-kit"), Some("ADMIN"))],
        );

        // Keyed by lowercased repository name, teams in slug order.
        let widget: Vec<(&str, &str)> = access["widget-kit"]
            .iter()
            .map(|(team, permission)| (team.as_str(), permission.as_str()))
            .collect();
        assert_eq!(widget, [("admins", "admin"), ("maintainers", "maintain")]);
        assert_eq!(access["docs"]["maintainers"], "write");
        assert_eq!(access.len(), 2);
    }

    #[test]
    fn filters_are_described_in_lowercase() {
        let described = filter(Visibility::Internal, Archived::Only, Forks::Exclude).describe();
        assert_eq!(described.visibility, "internal");
        assert_eq!(described.archived, "only");
        assert_eq!(described.forks, "exclude");
    }
}
