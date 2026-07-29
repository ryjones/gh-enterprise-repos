mod client;
mod collect;
mod model;
mod yaml;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures::stream::{self, StreamExt};

use client::GithubClient;
use collect::{Archived, Collector, Filter, Forks, OrgSnapshot, Visibility};
use model::*;

/// Export the repositories of every organization in a GitHub enterprise as
/// YAML — one file per enterprise, public non-archived repositories by default.
#[derive(Debug, Parser)]
#[command(name = "gh-enterprise-repos", version, about, long_about = None)]
struct Args {
    /// Enterprise slug. Repeatable; each one gets its own YAML file.
    #[arg(short, long, value_name = "SLUG", required = true)]
    enterprise: Vec<String>,

    /// Directory to write `<enterprise>.yaml` into.
    #[arg(short = 'd', long, value_name = "DIR", default_value = ".")]
    output_dir: PathBuf,

    /// Write a single enterprise's YAML here instead, or to stdout with `-`.
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

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();

    let mut enterprises: Vec<String> = args
        .enterprise
        .iter()
        .map(|slug| slug.trim().trim_matches('/').to_string())
        .filter(|slug| !slug.is_empty())
        .collect();
    enterprises.sort_by_key(|slug| slug.to_lowercase());
    enterprises.dedup_by_key(|slug| slug.to_lowercase());
    if enterprises.is_empty() {
        bail!("pass --enterprise <slug>");
    }
    if args.output.is_some() && enterprises.len() > 1 {
        bail!("--output writes one file; use --output-dir for several enterprises");
    }
    for slug in &enterprises {
        if slug.contains('/') || slug.contains('\\') || slug.starts_with('.') {
            bail!("`{slug}` is not a usable enterprise slug");
        }
    }

    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .map_err(|_| {
            anyhow::anyhow!("set GITHUB_TOKEN (or GH_TOKEN) to a token with read:enterprise")
        })?;
    if token.trim().is_empty() {
        bail!("GITHUB_TOKEN is set but empty");
    }

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

    let client = GithubClient::new(&api_url, token.trim(), args.max_retries)?;
    let collector = Collector::new(&client, args.filter(), args.topics, args.batch_size);

    let viewer = collector.viewer_login().await?;
    eprintln!("Authenticated as {viewer} at {api_url}");

    let writing_to_stdout = args.output.as_deref() == Some(Path::new("-"));
    if args.output.is_none() {
        std::fs::create_dir_all(&args.output_dir)
            .with_context(|| format!("failed to create {}", args.output_dir.display()))?;
    }

    let mut failed_enterprises = 0usize;
    for slug in &enterprises {
        match export_enterprise(&args, &collector, &api_url, slug).await {
            Ok(report) => {
                let yaml = yaml::to_string(&report).context("failed to serialize YAML")?;
                if writing_to_stdout {
                    std::io::stdout().lock().write_all(yaml.as_bytes())?;
                } else {
                    let path = match &args.output {
                        Some(path) => path.clone(),
                        None => args.output_dir.join(format!("{slug}.yaml")),
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
                failed_enterprises += 1;
                eprintln!("error: enterprise `{slug}`: {err:#}");
            }
        }
    }

    let rate = client.rate_state();
    if let (Some(remaining), Some(limit)) = (rate.remaining, rate.limit) {
        eprintln!("Rate limit: {remaining}/{limit} points remaining");
    }
    if failed_enterprises == enterprises.len() {
        bail!("every enterprise failed to export");
    }
    if failed_enterprises > 0 {
        eprintln!("Warning: {failed_enterprises} enterprise(s) failed");
    }
    Ok(())
}

/// Query one enterprise and assemble its report. One unreadable organization is
/// recorded and the rest still export; every organization failing is an error.
async fn export_enterprise(
    args: &Args,
    collector: &Collector<'_>,
    api_url: &str,
    slug: &str,
) -> Result<Report> {
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

    Ok(build_report(args, api_url, slug, succeeded, failed))
}

fn build_report(
    args: &Args,
    api_url: &str,
    enterprise: &str,
    snapshots: Vec<OrgSnapshot>,
    mut failed_orgs: Vec<String>,
) -> Report {
    let mut org_logins: Vec<String> = Vec::new();
    let mut repositories: Vec<Repository> = Vec::new();

    for snapshot in snapshots {
        org_logins.push(snapshot.login.clone());
        for repo in snapshot.repositories {
            repositories.push(repository(&snapshot.login, repo));
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

    Report {
        source: Source {
            api_url: api_url.to_string(),
            enterprise: enterprise.to_string(),
            filters: args.filter().describe(),
        },
        totals: Totals {
            organizations: org_logins.len(),
            repositories: repositories.len(),
        },
        organizations: org_logins,
        organizations_without_repository_data: failed_orgs,
        repositories,
    }
}

fn repository(org: &str, repo: RepoNode) -> Repository {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    fn report(snapshots: Vec<OrgSnapshot>) -> Report {
        build_report(
            &args(&[]),
            "https://api.github.com/graphql",
            "example",
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
            "example",
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
            "example",
            vec![snapshot("acme", vec![repo_node("one")])],
            Vec::new(),
        );
        assert_eq!(report.source.filters.visibility, "all");
        assert_eq!(report.source.filters.archived, "include");
        assert_eq!(report.source.filters.forks, "exclude");
        assert_eq!(report.source.enterprise, "example");
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
        assert!(
            parsed
                .get("organizations_without_repository_data")
                .is_none()
        );
    }
}
