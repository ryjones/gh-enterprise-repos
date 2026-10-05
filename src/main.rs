mod client;
mod collect;
mod model;
mod yaml;

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::stream::{self, StreamExt};

use client::GithubClient;
use collect::{Archived, Collector, Filter, Forks, OrgSnapshot, Visibility};
use model::*;

/// Export the repositories of every organization in a GitHub enterprise — or
/// of a single organization — as YAML: one file per enterprise or organization,
/// public non-archived repositories by default.
#[derive(Debug, Parser)]
#[command(name = "gh-enterprise-repos", version, about, long_about = None)]
struct Args {
    /// Enterprise slug. Repeatable; each one gets its own YAML file.
    #[arg(short, long, value_name = "SLUG", required_unless_present = "org")]
    enterprise: Vec<String>,

    /// Organization login, for one organization without going through its
    /// enterprise. Repeatable; each one gets its own YAML file.
    #[arg(long, value_name = "LOGIN")]
    org: Vec<String>,

    /// Directory to write `<enterprise>.yaml` or `<org>.yaml` into.
    #[arg(short = 'd', long, value_name = "DIR", default_value = ".")]
    output_dir: PathBuf,

    /// Write a single export's YAML here instead, or to stdout with `-`.
    #[arg(short, long, value_name = "FILE", conflicts_with = "output_dir")]
    output: Option<PathBuf>,

    /// Which repositories to include by visibility.
    #[arg(long, value_enum, default_value_t = Visibility::Public)]
    visibility: Visibility,

    /// What to do with archived repositories.
    #[arg(long, value_enum, default_value_t = Archived::Exclude)]
    archived: Archived,

    /// What to do with forks.
    #[arg(long, value_enum, default_value_t = Forks::Include)]
    forks: Forks,

    /// Include repository topics, which cost an extra nested lookup per repo.
    #[arg(long)]
    topics: bool,

    /// Include the teams with access to each repository and their permission,
    /// which costs a walk of every team in the organization.
    #[arg(long)]
    teams: bool,

    /// GraphQL endpoint. Defaults to github.com, or to the GitHub Enterprise
    /// Server endpoint derived from --hostname.
    #[arg(long, value_name = "URL")]
    api_url: Option<String>,

    /// GitHub Enterprise Server hostname, e.g. ghe.example.com.
    #[arg(long, value_name = "HOST", conflicts_with = "api_url")]
    hostname: Option<String>,

    /// Organizations queried at once.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=16))]
    concurrency: u16,

    /// Retries per request before giving up (rate limits, 5xx, timeouts).
    #[arg(long, default_value_t = 5)]
    max_retries: u32,

    /// Items requested per cursor fetch. Lower this if a large instance times
    /// out; pagination itself is always cursor-driven.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
    batch_size: u32,
}

impl Args {
    fn filter(&self) -> Filter {
        Filter {
            visibility: self.visibility,
            archived: self.archived,
            forks: self.forks,
        }
    }
}

/// What one output file covers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Enterprise(String),
    Org(String),
}

impl Target {
    fn name(&self) -> &str {
        match self {
            Target::Enterprise(name) | Target::Org(name) => name,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Target::Enterprise(_) => "enterprise",
            Target::Org(_) => "organization",
        }
    }
}

/// Trim, drop blanks, and sort and de-duplicate case-insensitively.
fn normalize(names: &[String]) -> Vec<String> {
    let mut names: Vec<String> = names
        .iter()
        .map(|name| name.trim().trim_matches('/').to_string())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup_by_key(|name| name.to_lowercase());
    names
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();

    let enterprises = normalize(&args.enterprise);
    let orgs = normalize(&args.org);
    // Both kinds are written as `<name>.yaml`, so one name cannot be both.
    if let Some(both) = orgs.iter().find(|org| {
        enterprises
            .iter()
            .any(|slug| slug.eq_ignore_ascii_case(org))
    }) {
        bail!(
            "`{both}` is given as both an enterprise and an organization, which would \
             write the same file twice; run them separately"
        );
    }
    let targets: Vec<Target> = enterprises
        .into_iter()
        .map(Target::Enterprise)
        .chain(orgs.into_iter().map(Target::Org))
        .collect();
    if targets.is_empty() {
        bail!("pass --enterprise <slug> or --org <login>");
    }
    if args.output.is_some() && targets.len() > 1 {
        bail!("--output writes one file; use --output-dir for several exports");
    }
    for target in &targets {
        let name = target.name();
        if name.contains('/') || name.contains('\\') || name.starts_with('.') {
            bail!("`{name}` is not a usable {} name", target.kind());
        }
    }

    let (token, token_from) = resolve_token(args.hostname.as_deref())?;

    let api_url = match (&args.api_url, &args.hostname) {
        (Some(url), _) => url.clone(),
        (None, Some(host)) => {
            let host = host.trim_end_matches('/');
            if host.starts_with("http://") || host.starts_with("https://") {
                format!("{host}/api/graphql")
            } else {
                format!("https://{host}/api/graphql")
            }
        }
        (None, None) => "https://api.github.com/graphql".to_string(),
    };

    let client = GithubClient::new(&api_url, &token, args.max_retries)?;
    let collector = Collector::new(
        &client,
        args.filter(),
        args.topics,
        args.teams,
        args.batch_size,
    );

    let viewer = collector.viewer_login().await?;
    eprintln!("Authenticated as {viewer} at {api_url} (token from {token_from})");

    let writing_to_stdout = args.output.as_deref() == Some(Path::new("-"));
    if args.output.is_none() {
        std::fs::create_dir_all(&args.output_dir)
            .with_context(|| format!("failed to create {}", args.output_dir.display()))?;
    }

    let mut failed_targets = 0usize;
    for target in &targets {
        let name = target.name();
        match export_target(&args, &collector, &api_url, target).await {
            Ok(mut report) => {
                report.source.authenticated_as = Some(viewer.clone());
                report.source.token_scopes = client.oauth_scopes();
                for note in &report.notes {
                    eprintln!("Warning: {note}");
                }
                let yaml = yaml::to_string(&report).context("failed to serialize YAML")?;
                if writing_to_stdout {
                    std::io::stdout().lock().write_all(yaml.as_bytes())?;
                } else {
                    let path = match &args.output {
                        Some(path) => path.clone(),
                        None => args.output_dir.join(format!("{name}.yaml")),
                    };
                    std::fs::write(&path, &yaml)
                        .with_context(|| format!("failed to write {}", path.display()))?;
                    eprintln!(
                        "Wrote {} ({} repositories across {} organizations)",
                        path.display(),
                        report.totals.repositories,
                        report.totals.organizations
                    );
                }
            }
            Err(err) => {
                failed_targets += 1;
                eprintln!("error: {} `{name}`: {err:#}", target.kind());
            }
        }
    }

    let rate = client.rate_state();
    if let (Some(remaining), Some(limit)) = (rate.remaining, rate.limit) {
        eprintln!("Rate limit: {remaining}/{limit} points remaining");
    }
    if failed_targets == targets.len() {
        bail!("nothing was exported: every enterprise and organization failed");
    }
    if failed_targets > 0 {
        eprintln!("Warning: {failed_targets} export(s) failed");
    }
    Ok(())
}

/// Query one enterprise or organization and assemble its report.
async fn export_target(
    args: &Args,
    collector: &Collector<'_>,
    api_url: &str,
    target: &Target,
) -> Result<Report> {
    let (snapshots, failed) = match target {
        Target::Enterprise(slug) => enterprise_snapshots(args, collector, slug).await?,
        Target::Org(login) => {
            eprintln!("Listing repositories in organization `{login}` …");
            (vec![collector.org_snapshot(login).await?], Vec::new())
        }
    };
    Ok(build_report(args, api_url, target, snapshots, failed))
}

/// Read every organization in an enterprise. One unreadable organization is
/// recorded and the rest still export; every organization failing is an error.
async fn enterprise_snapshots(
    args: &Args,
    collector: &Collector<'_>,
    slug: &str,
) -> Result<(Vec<OrgSnapshot>, Vec<String>)> {
    eprintln!("Listing organizations in enterprise `{slug}` …");
    let mut orgs = collector.enterprise_orgs(slug).await?;
    orgs.sort_by_key(|o| o.to_lowercase());
    orgs.dedup_by_key(|o| o.to_lowercase());
    eprintln!("  found {} organization(s)", orgs.len());
    if orgs.is_empty() {
        bail!("enterprise `{slug}` has no organizations visible to this token");
    }

    // Results arrive in completion order, so each one carries its own login
    // rather than being matched back against the input list by position.
    let total = orgs.len();
    let snapshots: Vec<(&String, Result<OrgSnapshot>)> = stream::iter(orgs.iter().enumerate())
        .map(|(index, login)| async move {
            eprintln!("[{}/{total}] {slug}/{login}", index + 1);
            (login, collector.org_snapshot(login).await)
        })
        .buffer_unordered(args.concurrency as usize)
        .collect()
        .await;

    let mut succeeded: Vec<OrgSnapshot> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for (login, snapshot) in snapshots {
        match snapshot {
            Ok(snapshot) => succeeded.push(snapshot),
            Err(err) => {
                failed.push(login.clone());
                eprintln!("  warning: {err:#}");
            }
        }
    }
    if succeeded.is_empty() {
        bail!("every organization in `{slug}` failed to query");
    }

    Ok((succeeded, failed))
}

fn build_report(
    args: &Args,
    api_url: &str,
    target: &Target,
    snapshots: Vec<OrgSnapshot>,
    mut failed_orgs: Vec<String>,
) -> Report {
    let mut org_logins: Vec<String> = Vec::new();
    let mut repositories: Vec<Repository> = Vec::new();

    for snapshot in snapshots {
        org_logins.push(snapshot.login.clone());
        let mut access = snapshot.teams;
        for repo in snapshot.repositories {
            let teams = access
                .as_mut()
                .and_then(|access| access.remove(&repo.name.to_lowercase()))
                .unwrap_or_default();
            repositories.push(repository(&snapshot.login, repo, teams));
        }
    }

    // Orgs that failed are still part of the enterprise, so they stay in the
    // listing — with a note that their repositories are missing.
    org_logins.extend(failed_orgs.iter().cloned());
    org_logins.sort_by_key(|o| o.to_lowercase());
    failed_orgs.sort_by_key(|o| o.to_lowercase());
    repositories.sort_by(|a, b| {
        a.org
            .to_lowercase()
            .cmp(&b.org.to_lowercase())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    // An org that answered and held nothing. Under a filtered run that is
    // ordinary -- most orgs have no internal repositories -- so the count is
    // recorded always and read as a symptom only when nothing was filtered out.
    let with_repos: HashSet<String> = repositories
        .iter()
        .map(|repo| repo.org.to_lowercase())
        .collect();
    let empty_orgs = org_logins
        .iter()
        .filter(|org| {
            !with_repos.contains(&org.to_lowercase())
                && !failed_orgs.iter().any(|failed| failed == *org)
        })
        .count();

    let filters = args.filter().describe();
    let unfiltered =
        filters.visibility == "all" && filters.archived == "include" && filters.forks == "include";
    let notes = visibility_note(empty_orgs, org_logins.len(), unfiltered)
        .into_iter()
        .collect();

    let (enterprise, organization) = match target {
        Target::Enterprise(slug) => (Some(slug.clone()), None),
        Target::Org(login) => (None, Some(login.clone())),
    };
    let without_teams = args
        .teams
        .then(|| repositories.iter().filter(|r| r.teams.is_empty()).count());

    Report {
        source: Source {
            api_url: api_url.to_string(),
            authenticated_as: None,
            token_scopes: None,
            enterprise,
            organization,
            filters,
        },
        totals: Totals {
            organizations: org_logins.len(),
            repositories: repositories.len(),
            organizations_without_repositories: empty_orgs,
            repositories_without_teams: without_teams,
        },
        organizations: org_logins,
        organizations_without_repository_data: failed_orgs,
        notes,
        repositories,
    }
}

/// Say so when a run that filtered nothing out still found organizations with
/// no repositories at all.
///
/// A token that cannot see into an organization is told it has no repositories
/// rather than being refused, so an unfiltered run that comes back empty for
/// several organizations is more likely short of access than short of code.
/// Under any filter the same emptiness is unremarkable, so nothing is claimed.
fn visibility_note(empty_orgs: usize, orgs: usize, unfiltered: bool) -> Option<String> {
    const MIN_ORGS: usize = 3;
    const MIN_PERCENT: f64 = 10.0;

    if !unfiltered || orgs == 0 || empty_orgs < MIN_ORGS {
        return None;
    }
    let percent = empty_orgs as f64 * 100.0 / orgs as f64;
    if percent < MIN_PERCENT {
        return None;
    }
    Some(format!(
        "organizations_without_repositories: {empty_orgs} of {orgs} organizations ({percent:.1}%) \
         held no repository, although this run filtered none out. An organization the token \
         cannot see into answers with an empty list rather than an error, so check these \
         against the enterprise before reading them as empty."
    ))
}

fn repository(org: &str, repo: RepoNode, teams: BTreeMap<String, String>) -> Repository {
    let mut topics: Vec<String> = repo
        .repository_topics
        .map(|connection| {
            connection
                .nodes
                .into_iter()
                .flatten()
                .filter_map(|node| node.topic)
                .map(|topic| topic.name)
                .collect()
        })
        .unwrap_or_default();
    topics.sort_by_key(|t| t.to_lowercase());
    topics.dedup();

    Repository {
        org: org.to_string(),
        name: repo.name,
        full_name: text(repo.name_with_owner),
        url: text(repo.url),
        description: text(repo.description),
        visibility: text(repo.visibility),
        archived: repo.is_archived.unwrap_or(false),
        fork: repo.is_fork.unwrap_or(false),
        template: repo.is_template.unwrap_or(false),
        empty: repo.is_empty.unwrap_or(false),
        default_branch: repo.default_branch_ref.map(|r| r.name),
        language: repo.primary_language.map(|l| l.name),
        // `NOASSERTION` is what GitHub reports for a license it recognizes but
        // cannot map to SPDX; the human-readable name says more.
        license: repo.license_info.and_then(|license| {
            match license.spdx_id.filter(|id| id != "NOASSERTION") {
                Some(spdx) => Some(spdx),
                None => license.name,
            }
        }),
        stars: repo.stargazer_count,
        forks: repo.fork_count,
        topics,
        teams,
        created_at: repo.created_at,
        updated_at: repo.updated_at,
        pushed_at: repo.pushed_at,
    }
}

/// Blank strings are absent rather than empty in the output.
fn text(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Find a token: the environment first, then whatever `gh` is logged in as.
///
/// The environment wins because it is the explicit choice, but the CLI's own
/// credential is worth falling back to: it already carries the SSO
/// authorizations and organization grants that a hand-made PAT is given one at
/// a time, and an enterprise listing made with a token that cannot see an
/// organization leaves it out without saying so.
fn resolve_token(hostname: Option<&str>) -> Result<(String, String)> {
    for name in ["GITHUB_TOKEN", "GH_TOKEN"] {
        match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => {
                return Ok((value.trim().to_string(), name.to_string()));
            }
            Ok(_) => bail!("{name} is set but empty"),
            Err(_) => {}
        }
    }

    let mut command = std::process::Command::new("gh");
    command.arg("auth").arg("token");
    if let Some(host) = hostname {
        let host = host
            .trim_end_matches('/')
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        command.arg("--hostname").arg(host);
    }
    let output = command.output().map_err(|err| {
        anyhow::anyhow!(
            "set GITHUB_TOKEN (or GH_TOKEN) to a token with read:enterprise, \
             or log in with `gh auth login` (could not run gh: {err})"
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "set GITHUB_TOKEN (or GH_TOKEN) to a token with read:enterprise, or log in \
             with `gh auth login` (gh auth token failed: {})",
            stderr.trim()
        );
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        bail!("`gh auth token` returned nothing; run `gh auth login`");
    }
    Ok((token, "gh auth token".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_org_under_a_filter_says_nothing() {
        // Most orgs hold no internal repositories; that is not a symptom.
        assert_eq!(visibility_note(20, 54, false), None);
    }

    #[test]
    fn empty_orgs_in_an_unfiltered_run_are_noted() {
        assert_eq!(visibility_note(0, 54, true), None);
        // Two of fifty-four is ordinary; the floor keeps it quiet.
        assert_eq!(visibility_note(2, 54, true), None);
        // Under the percentage bar, though over the count floor.
        assert_eq!(visibility_note(3, 54, true), None);
        let note = visibility_note(6, 54, true).expect("6 of 54 should be noted");
        assert!(
            note.starts_with("organizations_without_repositories: 6 of 54 organizations (11.1%)")
        );
        assert!(note.contains("cannot see into"));
    }

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["gh-enterprise-repos", "--enterprise", "example"];
        argv.extend_from_slice(extra);
        Args::parse_from(argv)
    }

    fn repo_node(name: &str) -> RepoNode {
        RepoNode {
            name: name.to_string(),
            name_with_owner: Some(format!("acme/{name}")),
            url: Some(format!("https://github.com/acme/{name}")),
            description: Some(format!("the {name} repository")),
            visibility: Some("PUBLIC".into()),
            is_archived: Some(false),
            is_fork: Some(false),
            is_template: Some(false),
            is_empty: Some(false),
            stargazer_count: Some(7),
            fork_count: Some(2),
            created_at: Some("2020-01-01T00:00:00Z".into()),
            updated_at: Some("2026-01-01T00:00:00Z".into()),
            pushed_at: Some("2026-01-02T00:00:00Z".into()),
            default_branch_ref: Some(RefNode {
                name: "main".into(),
            }),
            primary_language: Some(NamedNode {
                name: "Rust".into(),
            }),
            license_info: Some(LicenseNode {
                spdx_id: Some("Apache-2.0".into()),
                name: Some("Apache License 2.0".into()),
            }),
            repository_topics: None,
        }
    }

    fn snapshot(login: &str, repos: Vec<RepoNode>) -> OrgSnapshot {
        OrgSnapshot {
            login: login.to_string(),
            repositories: repos,
            teams: None,
        }
    }

    fn example() -> Target {
        Target::Enterprise("example".into())
    }

    fn report(snapshots: Vec<OrgSnapshot>) -> Report {
        build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            &example(),
            snapshots,
            Vec::new(),
        )
    }

    #[test]
    fn repositories_are_ordered_by_org_then_name_case_insensitively() {
        let report = report(vec![
            snapshot("Zulu", vec![repo_node("beta"), repo_node("Alpha")]),
            snapshot("alpha", vec![repo_node("zeta")]),
        ]);
        let listed: Vec<String> = report
            .repositories
            .iter()
            .map(|r| format!("{}/{}", r.org, r.name))
            .collect();
        assert_eq!(listed, ["alpha/zeta", "Zulu/Alpha", "Zulu/beta"]);
        assert_eq!(report.organizations, ["alpha", "Zulu"]);
        assert_eq!(report.totals.repositories, 3);
        assert_eq!(report.totals.organizations, 2);
    }

    #[test]
    fn an_org_with_no_repositories_is_still_listed() {
        let report = report(vec![snapshot("empty-org", vec![])]);
        assert_eq!(report.organizations, ["empty-org"]);
        assert!(report.repositories.is_empty());
    }

    #[test]
    fn an_unreadable_org_is_listed_and_flagged() {
        let report = build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            &example(),
            vec![snapshot("readable", vec![repo_node("one")])],
            vec!["locked-down".into()],
        );
        assert_eq!(report.organizations, ["locked-down", "readable"]);
        assert_eq!(
            report.organizations_without_repository_data,
            ["locked-down"]
        );
        assert_eq!(report.totals.organizations, 2);
        assert_eq!(report.totals.repositories, 1);
    }

    #[test]
    fn topics_are_sorted_and_absent_when_not_requested() {
        let mut with_topics = repo_node("one");
        with_topics.repository_topics = Some(TopicConnection {
            nodes: vec![
                Some(RepositoryTopic {
                    topic: Some(NamedNode {
                        name: "rust".into(),
                    }),
                }),
                Some(RepositoryTopic {
                    topic: Some(NamedNode {
                        name: "Cryptography".into(),
                    }),
                }),
                Some(RepositoryTopic { topic: None }),
            ],
        });
        let report = report(vec![snapshot("acme", vec![with_topics, repo_node("two")])]);
        assert_eq!(report.repositories[0].topics, ["Cryptography", "rust"]);
        assert!(report.repositories[1].topics.is_empty());
    }

    #[test]
    fn unmappable_licenses_fall_back_to_the_license_name() {
        let mut other = repo_node("one");
        other.license_info = Some(LicenseNode {
            spdx_id: Some("NOASSERTION".into()),
            name: Some("Other".into()),
        });
        let report = report(vec![snapshot("acme", vec![other])]);
        assert_eq!(report.repositories[0].license.as_deref(), Some("Other"));
    }

    #[test]
    fn blank_and_missing_fields_are_omitted() {
        let mut sparse = repo_node("one");
        sparse.description = Some("   ".into());
        sparse.url = None;
        sparse.default_branch_ref = None;
        sparse.license_info = None;
        sparse.primary_language = None;
        let report = report(vec![snapshot("acme", vec![sparse])]);

        let repo = &report.repositories[0];
        assert_eq!(repo.description, None);
        assert_eq!(repo.url, None);
        assert_eq!(repo.default_branch, None);
        assert_eq!(repo.license, None);
        assert_eq!(repo.language, None);
    }

    #[test]
    fn the_filter_the_run_used_is_recorded() {
        let report = build_report(
            &args(&[
                "--visibility",
                "all",
                "--archived",
                "include",
                "--forks",
                "exclude",
            ]),
            "https://api.github.com/graphql",
            &example(),
            vec![snapshot("acme", vec![repo_node("one")])],
            Vec::new(),
        );
        assert_eq!(report.source.filters.visibility, "all");
        assert_eq!(report.source.filters.archived, "include");
        assert_eq!(report.source.filters.forks, "exclude");
        assert_eq!(report.source.enterprise.as_deref(), Some("example"));
        assert_eq!(report.source.organization, None);
    }

    #[test]
    fn an_organization_can_stand_in_for_an_enterprise() {
        let args = Args::parse_from(["gh-enterprise-repos", "--org", "acme"]);
        assert!(args.enterprise.is_empty());
        let report = build_report(
            &args,
            "https://api.github.com/graphql",
            &Target::Org("acme".into()),
            vec![snapshot("acme", vec![repo_node("one")])],
            Vec::new(),
        );
        assert_eq!(report.source.organization.as_deref(), Some("acme"));
        assert_eq!(report.source.enterprise, None);
        assert_eq!(report.organizations, ["acme"]);

        assert!(Args::try_parse_from(["gh-enterprise-repos"]).is_err());
    }

    #[test]
    fn names_are_trimmed_sorted_and_deduplicated() {
        let names = ["beta/", " Alpha ", "ALPHA", "", "beta"].map(String::from);
        assert_eq!(normalize(&names), ["Alpha", "beta"]);
    }

    #[test]
    fn teams_are_attached_by_repository_name_and_the_gaps_counted() {
        let mut with_teams = snapshot("acme", vec![repo_node("One"), repo_node("two")]);
        with_teams.teams = Some(
            [(
                "one".to_string(),
                [
                    ("maintainers".to_string(), "maintain".to_string()),
                    ("admins".to_string(), "admin".to_string()),
                ]
                .into(),
            )]
            .into(),
        );
        let report = build_report(
            &args(&["--teams"]),
            "https://api.github.com/graphql",
            &example(),
            vec![with_teams],
            Vec::new(),
        );

        let teams: Vec<(&str, &str)> = report.repositories[0]
            .teams
            .iter()
            .map(|(team, permission)| (team.as_str(), permission.as_str()))
            .collect();
        assert_eq!(teams, [("admins", "admin"), ("maintainers", "maintain")]);
        assert!(report.repositories[1].teams.is_empty());
        assert_eq!(report.totals.repositories_without_teams, Some(1));
    }

    #[test]
    fn without_the_flag_no_team_count_is_claimed() {
        let report = report(vec![snapshot("acme", vec![repo_node("one")])]);
        assert_eq!(report.totals.repositories_without_teams, None);
        let yaml = yaml::to_string(&report).expect("serializes");
        assert!(!yaml.contains("teams"));
    }

    #[test]
    fn yaml_round_trips_to_the_documented_shape() {
        let report = report(vec![snapshot("acme", vec![repo_node("one")])]);
        let yaml = serde_yaml_ng::to_string(&report).expect("serializes");
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).expect("parses");

        assert_eq!(parsed["source"]["enterprise"].as_str(), Some("example"));
        assert_eq!(
            parsed["source"]["filters"]["visibility"].as_str(),
            Some("public")
        );
        let repo = &parsed["repositories"][0];
        assert_eq!(repo["org"].as_str(), Some("acme"));
        assert_eq!(repo["name"].as_str(), Some("one"));
        assert_eq!(repo["full_name"].as_str(), Some("acme/one"));
        assert_eq!(repo["archived"].as_bool(), Some(false));
        assert_eq!(repo["license"].as_str(), Some("Apache-2.0"));
        assert_eq!(repo["default_branch"].as_str(), Some("main"));
        // False flags and empty lists are absent, not null.
        assert!(repo.get("template").is_none());
        assert!(repo.get("topics").is_none());
        assert!(repo.get("teams").is_none());
        assert!(parsed["source"].get("organization").is_none());
        assert!(
            parsed
                .get("organizations_without_repository_data")
                .is_none()
        );
    }
}
