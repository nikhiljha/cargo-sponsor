use anyhow::{Context, Result};
use cargo_metadata::{MetadataCommand, Package};
use clap::{Parser, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use url::Url;

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
#[command(author, version, about = "Find sponsorship links for your dependencies")]
struct Args {
    #[arg(long, default_value = ".")]
    manifest_path: PathBuf,
    #[arg(long, default_value = "rich")]
    output: OutputFormat,
    #[arg(long)]
    top_level_only: bool,
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

async fn get_repo_sponsor_info(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    token: Option<&str>,
) -> Result<Option<RepoInfo>> {
    let token = match token {
        Some(t) => t,
        None => return Ok(None),
    };

    let query = r#"
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
    "#;

    let body = serde_json::json!({
        "query": query,
        "variables": { "owner": owner, "repo": repo }
    });

    let resp = client
        .post("https://api.github.com/graphql")
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", "cargo-sponsor")
        .json(&body)
        .send()
        .await?;

    let data: GitHubResponse = resp.json().await?;

    if let Some(data) = data.data {
        if let Some(repo) = data.repository {
            let links: Vec<String> = repo.funding_links.into_iter().map(|f| f.url).collect();
            let sponsor_count = if repo.owner.has_sponsors_listing {
                repo.owner.sponsors.map(|s| s.total_count)
            } else {
                None
            };
            return Ok(Some(RepoInfo {
                funding_links: links,
                sponsor_count,
            }));
        }
    }

    Ok(None)
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

#[tokio::main]
async fn main() -> Result<()> {
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

    let client = reqwest::Client::new();
    let token = std::env::var("GITHUB_TOKEN").ok().or_else(|| {
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
    });

    if token.is_none() {
        eprintln!("Note: Set GITHUB_TOKEN env var or install/auth the GitHub CLI for sponsor count info and FUNDING.yml parsing");
        eprintln!();
    }

    let mut seen_repos: HashMap<String, String> = HashMap::new();
    let mut results: Vec<SponsorInfo> = Vec::new();

    let root_packages: Vec<_> = metadata
        .workspace_members
        .iter()
        .filter_map(|id| metadata.packages.iter().find(|p| &p.id == id))
        .map(|p| p.name.clone())
        .collect();

    let direct_deps: std::collections::HashSet<_> = if args.top_level_only {
        metadata
            .workspace_members
            .iter()
            .filter_map(|id| metadata.packages.iter().find(|p| &p.id == id))
            .flat_map(|p| p.dependencies.iter().map(|d| d.name.clone()))
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let deps: Vec<&Package> = metadata
        .packages
        .iter()
        .filter(|p| !root_packages.contains(&p.name))
        .filter(|p| !args.top_level_only || direct_deps.contains(&p.name))
        .collect();

    let pb = ProgressBar::new(deps.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} Retrieving GitHub sponsor information... [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );

    for package in deps {
        pb.set_message(package.name.clone());
        pb.inc(1);
        let repo_url = match &package.repository {
            Some(url) => url,
            None => continue,
        };

        let (repo_owner, repo_name) = match extract_github_repo(repo_url) {
            Some(r) => r,
            None => continue,
        };

        if seen_repos.contains_key(&repo_owner) {
            continue;
        }

        let repo_info = get_repo_sponsor_info(&client, &repo_owner, &repo_name, token.as_deref())
            .await
            .unwrap_or(None);

        let (links, sponsor_count) = match repo_info {
            Some(info) => (info.funding_links, info.sponsor_count),
            None => (vec![], None),
        };

        seen_repos.insert(repo_owner.clone(), links.first().cloned().unwrap_or_default());

        if !links.is_empty() {
            results.push(SponsorInfo {
                name: package.name.clone(),
                repository: repo_url.clone(),
                sponsor_links: links,
                sponsor_count,
            });
        }
    }

    pb.finish_and_clear();

    match args.output {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&results)?);
        }
        OutputFormat::Rich => {
            if results.is_empty() {
                println!("No sponsorable dependencies found.");
            } else {
                println!("\n  {}\n", "💝 Sponsorable Dependencies".cyan().bold());
                println!("  Found {} projects you can support:\n", results.len().bold());

                let name_width = results.iter().map(|r| r.name.len()).max().unwrap_or(10).max(10);
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

                for info in &results {
                    let sponsor_str = info
                        .sponsor_count
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let link = info.sponsor_links.first().map(|s| s.as_str()).unwrap_or("-");
                    println!(
                        "  {:<name_width$}  {:<sponsors_width$}  {}",
                        info.name.yellow(),
                        sponsor_str.dimmed(),
                        link.blue().underline(),
                    );
                }
                println!();
            }
        }
    }

    Ok(())
}
