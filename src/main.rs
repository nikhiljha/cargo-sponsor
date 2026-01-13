use anyhow::{Context, Result};
use cargo_metadata::{MetadataCommand, Package};
use clap::{Parser, ValueEnum};
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};
use url::Url;

const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";
const USER_AGENT: &str = "cargo-sponsor";

#[derive(Parser)]
#[command(name = "cargo")]
#[command(bin_name = "cargo")]
enum Cargo {
    Sponsor(Args),
}

#[derive(Clone, Copy, Default, ValueEnum)]
enum OutputFormat {
    #[default]
    Rich,
    Json,
}

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Find sponsorship links for your dependencies"
)]
struct Args {
    #[arg(long, default_value = ".")]
    manifest_path: PathBuf,
    #[arg(long, default_value = "rich")]
    output: OutputFormat,
    #[arg(long)]
    top_level_only: bool,
    #[arg(long, default_value = "10")]
    concurrency: usize,
}

#[derive(Debug, Serialize)]
struct SponsorInfo {
    name: String,
    repository: String,
    sponsor_links: Vec<String>,
    sponsor_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct GitHubResponse {
    data: Option<GitHubData>,
}

#[derive(Debug, Deserialize)]
struct GitHubData {
    repository: Option<RepositoryData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryData {
    funding_links: Vec<FundingLink>,
    owner: OwnerData,
}

#[derive(Debug, Deserialize)]
struct FundingLink {
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnerData {
    has_sponsors_listing: bool,
    sponsors: Option<SponsorConnection>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SponsorConnection {
    total_count: u32,
}

struct RepoInfo {
    funding_links: Vec<String>,
    sponsor_count: Option<u32>,
}

const MAX_RETRIES: u32 = 3;

async fn get_repo_sponsor_info(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    token: Option<&Arc<str>>,
) -> Result<Option<RepoInfo>> {
    let Some(token) = token else {
        return Ok(None);
    };

    let query = r"
        query($owner: String!, $repo: String!) {
            repository(owner: $owner, name: $repo) {
                fundingLinks { url }
                owner {
                    ... on User {
                        hasSponsorsListing
                        sponsors { totalCount }
                    }
                    ... on Organization {
                        hasSponsorsListing
                        sponsors { totalCount }
                    }
                }
            }
        }
    ";

    let body = serde_json::json!({
        "query": query,
        "variables": { "owner": owner, "repo": repo }
    });

    let mut retries = 0;
    loop {
        let resp = client
            .post(GITHUB_GRAPHQL_URL)
            .header("Authorization", format!("Bearer {token}"))
            .header("User-Agent", USER_AGENT)
            .json(&body)
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            if retries >= MAX_RETRIES {
                anyhow::bail!("Rate limited after {MAX_RETRIES} retries for {owner}/{repo}");
            }

            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or_else(|| 2u64.pow(retries));

            debug!(
                "Rate limited for {}/{}, waiting {}s (retry {}/{})",
                owner,
                repo,
                retry_after,
                retries + 1,
                MAX_RETRIES
            );
            tokio::time::sleep(Duration::from_secs(retry_after)).await;
            retries += 1;
            continue;
        }

        if !resp.status().is_success() {
            anyhow::bail!("GitHub API error for {}/{}: {}", owner, repo, resp.status());
        }

        let data: GitHubResponse = resp.json().await?;

        if let Some(data) = data.data
            && let Some(repo_data) = data.repository
        {
            let links: Vec<String> = repo_data.funding_links.into_iter().map(|f| f.url).collect();
            let sponsor_count = if repo_data.owner.has_sponsors_listing {
                repo_data.owner.sponsors.map(|s| s.total_count)
            } else {
                None
            };
            return Ok(Some(RepoInfo {
                funding_links: links,
                sponsor_count,
            }));
        }

        return Ok(None);
    }
}

fn extract_github_repo(repo_url: &str) -> Option<(String, String)> {
    let url = Url::parse(repo_url).ok()?;
    if url.host_str()? != "github.com" {
        return None;
    }
    let segments: Vec<_> = url.path_segments()?.collect();
    if segments.len() < 2 {
        return None;
    }
    let repo = segments[1].trim_end_matches(".git").to_string();
    Some((segments[0].to_string(), repo))
}

fn get_github_token() -> Option<Arc<str>> {
    std::env::var("GITHUB_TOKEN")
        .ok()
        .or_else(|| {
            std::process::Command::new("gh")
                .args(["auth", "token"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| {
                    String::from_utf8(output.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                })
                .filter(|s| !s.is_empty())
        })
        .map(Arc::from)
}

fn collect_repos_to_fetch(
    deps: &[&Package],
) -> Vec<(String, String, String, String)> {
    let mut seen_repos: HashSet<(String, String)> = HashSet::new();
    let mut to_fetch = Vec::new();

    for package in deps {
        let Some(repo_url) = &package.repository else {
            continue;
        };

        let Some((repo_owner, repo_name)) = extract_github_repo(repo_url) else {
            continue;
        };

        if seen_repos.contains(&(repo_owner.clone(), repo_name.clone())) {
            continue;
        }
        seen_repos.insert((repo_owner.clone(), repo_name.clone()));
        to_fetch.push((
            package.name.to_string(),
            repo_url.clone(),
            repo_owner,
            repo_name,
        ));
    }

    to_fetch
}

fn process_result(
    results: &mut Vec<SponsorInfo>,
    pkg_name: String,
    repo_url: String,
    owner: &str,
    repo: &str,
    result: Result<Option<RepoInfo>>,
) {
    match result {
        Ok(Some(info)) if !info.funding_links.is_empty() => {
            results.push(SponsorInfo {
                name: pkg_name,
                repository: repo_url,
                sponsor_links: info.funding_links,
                sponsor_count: info.sponsor_count,
            });
        }
        Ok(_) => {}
        Err(e) => {
            warn!("Failed to fetch sponsor info for {owner}/{repo}: {e}");
        }
    }
}

fn print_results(results: &[SponsorInfo]) {
    if results.is_empty() {
        println!("No sponsorable dependencies found.");
        return;
    }

    println!("\n  {}\n", "💝 Sponsorable Dependencies".cyan().bold());
    println!(
        "  Found {} projects you can support:\n",
        results.len().bold()
    );

    let name_width = results
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(10)
        .max(10);
    let sponsors_width = 10;

    println!(
        "  {:<name_width$}  {:<sponsors_width$}  {}",
        "Package".bold(),
        "Sponsors".bold(),
        "Link".bold(),
    );
    println!(
        "  {:<name_width$}  {:<sponsors_width$}  {}",
        "─".repeat(name_width),
        "─".repeat(sponsors_width),
        "─".repeat(40),
    );

    for info in results {
        let sponsor_str = info
            .sponsor_count
            .map_or_else(|| "-".to_string(), |c| c.to_string());
        let link = info
            .sponsor_links
            .first()
            .map_or("-", std::string::String::as_str);
        println!(
            "  {:<name_width$}  {:<sponsors_width$}  {}",
            info.name.yellow(),
            sponsor_str.dimmed(),
            link.blue().underline(),
        );
    }
    println!();
}

async fn fetch_sponsor_info(
    client: &reqwest::Client,
    token: Option<&Arc<str>>,
    to_fetch: Vec<(String, String, String, String)>,
    concurrency: usize,
) -> Vec<SponsorInfo> {
    let pb = ProgressBar::new(to_fetch.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} Retrieving GitHub sponsor information... [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .expect("invalid progress bar template")
            .progress_chars("#>-"),
    );

    let mut results: Vec<SponsorInfo> = Vec::new();
    let mut futures = FuturesUnordered::new();

    for (pkg_name, repo_url, owner, repo) in to_fetch {
        let client = client.clone();
        let token = token.cloned();
        let pb = pb.clone();

        futures.push(async move {
            pb.set_message(pkg_name.clone());
            let result = get_repo_sponsor_info(&client, &owner, &repo, token.as_ref()).await;
            pb.inc(1);
            (pkg_name, repo_url, owner, repo, result)
        });

        if futures.len() >= concurrency
            && let Some((pkg_name, repo_url, owner, repo, result)) = futures.next().await
        {
            process_result(&mut results, pkg_name, repo_url, &owner, &repo, result);
        }
    }

    while let Some((pkg_name, repo_url, owner, repo, result)) = futures.next().await {
        process_result(&mut results, pkg_name, repo_url, &owner, &repo, result);
    }

    pb.finish_and_clear();
    results
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let Cargo::Sponsor(args) = Cargo::parse();

    let manifest_path = if args.manifest_path.is_dir() {
        args.manifest_path.join("Cargo.toml")
    } else {
        args.manifest_path
    };

    let metadata = MetadataCommand::new()
        .manifest_path(&manifest_path)
        .exec()
        .context("Failed to get cargo metadata")?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let token = get_github_token();

    if token.is_none() {
        eprintln!(
            "Note: Set GITHUB_TOKEN env var or install/auth the GitHub CLI for sponsor count info and FUNDING.yml parsing"
        );
        eprintln!();
    }

    let root_packages: Vec<_> = metadata
        .workspace_members
        .iter()
        .filter_map(|id| metadata.packages.iter().find(|p| &p.id == id))
        .map(|p| p.name.clone())
        .collect();

    let direct_deps: HashSet<_> = if args.top_level_only {
        metadata
            .workspace_members
            .iter()
            .filter_map(|id| metadata.packages.iter().find(|p| &p.id == id))
            .flat_map(|p| p.dependencies.iter().map(|d| d.name.clone()))
            .collect()
    } else {
        HashSet::new()
    };

    let deps: Vec<&Package> = metadata
        .packages
        .iter()
        .filter(|p| !root_packages.contains(&p.name))
        .filter(|p| !args.top_level_only || direct_deps.contains(p.name.as_str()))
        .collect();

    let to_fetch = collect_repos_to_fetch(&deps);
    let results = fetch_sponsor_info(&client, token.as_ref(), to_fetch, args.concurrency).await;

    match args.output {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&results)?);
        }
        OutputFormat::Rich => {
            print_results(&results);
        }
    }

    Ok(())
}
