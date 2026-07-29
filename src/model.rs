use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GraphQL response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PageInfo {
    #[serde(rename = "hasNextPage")]
    pub has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ViewerData {
    pub viewer: ViewerNode,
}

#[derive(Debug, Deserialize)]
pub struct ViewerNode {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct OrgNode {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct OrgConnection {
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    #[serde(default)]
    pub nodes: Vec<Option<OrgNode>>,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseOrgsData {
    pub enterprise: Option<EnterpriseNode>,
}

#[derive(Debug, Deserialize)]
pub struct EnterpriseNode {
    pub organizations: OrgConnection,
}

#[derive(Debug, Deserialize)]
pub struct OrgReposData {
    pub organization: Option<OrgReposNode>,
}

#[derive(Debug, Deserialize)]
pub struct OrgReposNode {
    pub repositories: RepoConnection,
}

#[derive(Debug, Deserialize)]
pub struct RepoConnection {
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    #[serde(default)]
    pub nodes: Vec<Option<RepoNode>>,
}

/// One repository as GitHub returns it. Everything past `name` is optional, so
/// a field the token cannot see nulls out instead of failing the whole page.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoNode {
    pub name: String,
    pub name_with_owner: Option<String>,
    pub url: Option<String>,
    pub description: Option<String>,
    /// `PUBLIC`, `PRIVATE` or `INTERNAL`.
    pub visibility: Option<String>,
    pub is_archived: Option<bool>,
    pub is_fork: Option<bool>,
    pub is_template: Option<bool>,
    pub is_empty: Option<bool>,
    pub stargazer_count: Option<u64>,
    pub fork_count: Option<u64>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub pushed_at: Option<String>,
    pub default_branch_ref: Option<RefNode>,
    pub primary_language: Option<NamedNode>,
    pub license_info: Option<LicenseNode>,
    /// Absent entirely when topics were not requested.
    pub repository_topics: Option<TopicConnection>,
}

#[derive(Debug, Deserialize)]
pub struct RefNode {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct NamedNode {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct LicenseNode {
    #[serde(rename = "spdxId")]
    pub spdx_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TopicConnection {
    #[serde(default)]
    pub nodes: Vec<Option<RepositoryTopic>>,
}

#[derive(Debug, Deserialize)]
pub struct RepositoryTopic {
    pub topic: Option<NamedNode>,
}

// ---------------------------------------------------------------------------
// YAML output shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Report {
    pub source: Source,
    /// Org logins, ascending.
    pub organizations: Vec<String>,
    /// Orgs that could not be read at all, so none of their repositories are
    /// listed. Absent when every org was readable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub organizations_without_repository_data: Vec<String>,
    pub totals: Totals,
    /// Repositories, ordered by org login then repository name (both
    /// case-insensitive, ascending).
    pub repositories: Vec<Repository>,
}

#[derive(Debug, Serialize)]
pub struct Source {
    pub api_url: String,
    pub enterprise: String,
    /// The filters the run applied, echoed so a file explains itself.
    pub filters: Filters,
}

#[derive(Debug, Serialize)]
pub struct Filters {
    /// `public`, `private`, `internal` or `all`.
    pub visibility: String,
    /// `exclude`, `include` or `only`.
    pub archived: String,
    /// `include`, `exclude` or `only`.
    pub forks: String,
}

#[derive(Debug, Serialize)]
pub struct Totals {
    pub organizations: usize,
    pub repositories: usize,
}

#[derive(Debug, Serialize)]
pub struct Repository {
    pub org: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `PUBLIC`, `PRIVATE` or `INTERNAL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    pub archived: bool,
    pub fork: bool,
    /// Only emitted when true, so the common case stays quiet.
    #[serde(skip_serializing_if = "is_false")]
    pub template: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub empty: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stars: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forks: Option<u64>,
    /// Ascending; absent without `--topics`, or when the repo has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pushed_at: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}
