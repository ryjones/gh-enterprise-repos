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
}

pub struct Collector<'a> {
    client: &'a GithubClient,
    filter: Filter,
    with_topics: bool,
    batch_size: u32,
}

impl<'a> Collector<'a> {
    pub fn new(
        client: &'a GithubClient,
        filter: Filter,
        with_topics: bool,
        batch_size: u32,
    ) -> Self {
        Self {
            client,
            filter,
            with_topics,
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

        Ok(OrgSnapshot {
            login: login.to_string(),
            repositories: out,
        })
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

    #[test]
    fn filters_are_described_in_lowercase() {
        let described = filter(Visibility::Internal, Archived::Only, Forks::Exclude).describe();
        assert_eq!(described.visibility, "internal");
        assert_eq!(described.archived, "only");
        assert_eq!(described.forks, "exclude");
    }
}
