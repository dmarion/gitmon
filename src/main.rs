use clap::Parser;
use dirs;
use git2::Repository;
use lettre::{
    message::{header::ContentType, Mailbox},
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::process::Command;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
};

#[derive(Parser, Debug)]
#[command(name = "gitmon", version, author, about)]
struct Args {
    #[arg(short, long)]
    verbose: bool,

    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RepoEntry {
    Url(String),
    Config {
        url: String,
        branches: Option<Vec<String>>,
    },
}

impl RepoEntry {
    fn url(&self) -> &str {
        match self {
            RepoEntry::Url(u) => u,
            RepoEntry::Config { url, .. } => url,
        }
    }

    fn branches(&self) -> Option<&[String]> {
        match self {
            RepoEntry::Url(_) => None,
            RepoEntry::Config { branches, .. } => branches.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    repos: Vec<RepoEntry>,
    from: String,
    to: String,
    token: String,
    template_path: Option<String>,
    cache_dir: Option<String>,
    max_commits: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct State {
    last_seen: BTreeMap<String, String>,
}

struct CommitInfo {
    id: String,
    date: String,
    author: String,
    message: String,
    change_id: Option<String>,
}

struct RepoReport {
    url: String,
    branch: String,
    commits: Vec<CommitInfo>,
}

fn load_state(path: &PathBuf) -> State {
    if path.exists() {
        let data = fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        State::default()
    }
}

fn save_state(state: &State, path: &PathBuf) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        fs::write(path, json).ok();
    }
}

fn hash_repo_url(url: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(url.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn clone_or_update_repo(
    remote_url: &str,
    base_cache_dir: &PathBuf,
) -> Result<PathBuf, git2::Error> {
    let repo_hash = hash_repo_url(remote_url);
    let repo_dir = base_cache_dir.join(repo_hash);

    if repo_dir.exists() {
        debug!("Fetching updates for {}", remote_url);
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .arg("fetch")
            .arg("--all")
            .output();

        match output {
            Ok(out) if out.status.success() => debug!("Updated {}", remote_url),
            Ok(out) => eprintln!(
                "Git fetch failed: {}\n{}",
                remote_url,
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(e) => eprintln!("Git fetch error on {}: {}", remote_url, e),
        }

        Ok(repo_dir)
    } else {
        debug!("Cloning {}", remote_url);
        Repository::clone(remote_url, &repo_dir)?;
        Ok(repo_dir)
    }
}

fn get_all_remote_branches(repo_path: &PathBuf) -> Result<Vec<String>, git2::Error> {
    let repo = Repository::open(repo_path)?;
    let mut branches = Vec::new();

    for reference in repo.references_glob("refs/remotes/origin/*")? {
        let reference = reference?;
        if let Some(name) = reference.shorthand() {
            // name is like "origin/main" — strip the "origin/" prefix
            if let Some(branch) = name.strip_prefix("origin/") {
                if branch != "HEAD" {
                    branches.push(branch.to_string());
                }
            }
        }
    }

    Ok(branches)
}

fn get_new_commits_since(
    repo_path: &PathBuf,
    branch: &str,
    last_seen: Option<&str>,
    max_commits: Option<usize>,
) -> Result<Vec<CommitInfo>, git2::Error> {
    let repo = Repository::open(repo_path)?;
    let mut revwalk = repo.revwalk()?;
    revwalk.push_ref(&format!("refs/remotes/origin/{}", branch))?;
    revwalk.set_sorting(git2::Sort::TIME)?;

    let mut commits = Vec::new();
    for oid in revwalk {
        if let Some(max) = max_commits {
            if commits.len() >= max {
                break;
            }
        }

        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let id_str = commit.id().to_string();

        if Some(id_str.as_str()) == last_seen {
            break;
        }

        let time = commit.time().seconds();
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(time, 0)
            .unwrap()
            .with_timezone(&chrono::Local);

        let mut change_id: Option<String> = None;
        for line in commit.message().unwrap_or("").lines() {
            if let Some(stripped) = line.strip_prefix("Change-Id:") {
                change_id = Some(stripped.trim().to_string());
                break;
            }
        }

        commits.push(CommitInfo {
            id: id_str,
            date: dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            author: commit.author().name().unwrap_or("Unknown").to_string(),
            message: commit.summary().unwrap_or("").to_string(),
            change_id: change_id,
        });
    }

    Ok(commits)
}

fn trim_after_domain(url: &str) -> &str {
    let url_no_scheme = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };

    match url_no_scheme.find('/') {
        Some(pos) => &url[..pos + (url.len() - url_no_scheme.len())],
        None => url,
    }
}

fn build_html_report_with_template(
    reports: &[RepoReport],
    template_path: Option<&str>,
) -> String {
    let mut tables = String::new();

    for report in reports {
        if report.commits.is_empty() {
            continue;
        }
        let heading = format!("{} [{}]", report.url, report.branch);
        let url = &report.url;
        tables.push_str(&format!(
            "<h2>Repository: {}</h2><table border=\"1\"><tr><th>ID</th><th>Date</th><th>Author</th><th>Message</th></tr>",
            heading
        ));
        for c in &report.commits {
            let patch_url = if url.contains("github.com") {
                format!("{}/commit/{}", url.trim_end_matches(".git"), c.id)
            } else if url.contains("gitlab.com") {
                format!("{}/-/commit/{}.patch", url.trim_end_matches(".git"), c.id)
            } else if url.contains("bitbucket.org") {
                format!("{}/commits/{}.patch", url.trim_end_matches(".git"), c.id)
            } else if url.contains("gerrit") && c.change_id.is_some() {
                format!(
                    "{}/r/q/{}",
                    trim_after_domain(url.trim_end_matches(".git")),
                    c.change_id.as_ref().unwrap()
                )
            } else {
                c.id.clone()
            };

            let id_link = if patch_url.starts_with("http") {
                format!("<a href=\"{}\">{}</a>", patch_url, c.id)
            } else {
                c.id.clone()
            };

            tables.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                id_link, c.date, c.author, c.message
            ));
        }
        tables.push_str("</table>");
    }

    if let Some(path) = template_path {
        if let Ok(template) = fs::read_to_string(path) {
            return template.replace("{{tables}}", &tables);
        }
    }

    format!(
        "<html><body><h1>Git Commit Report</h1>{}</body></html>",
        tables
    )
}

fn send_email(
    html_body: String,
    from: &str,
    to: &str,
    token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let email = Message::builder()
        .from(from.parse::<Mailbox>()?)
        .to(to.parse::<Mailbox>()?)
        .subject("Git Commit Notification")
        .header(ContentType::TEXT_HTML)
        .body(html_body)?;

    let creds = Credentials::new(from.to_string(), token.to_string());

    let mailer = SmtpTransport::relay("smtp.gmail.com")?
        .credentials(creds)
        .build();

    mailer.send(&email)?;
    Ok(())
}

fn load_config(provided_path: Option<&PathBuf>) -> Config {
    let resolved_path = if let Some(path) = provided_path {
        path.clone()
    } else {
        let base_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = dirs::home_dir().expect("Could not determine home directory");
                home.join(".config")
            });
        base_dir.join("gitmon").join("config.toml")
    };

    let content = std::fs::read_to_string(&resolved_path)
        .unwrap_or_else(|_| panic!("Failed to read config file at {:?}", resolved_path));

    toml::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse config TOML at {:?}: {}", resolved_path, e))
}

fn main() {
    let args = Args::parse();

    if args.verbose {
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Debug)
            .init();
    } else {
        env_logger::init();
    }

    let config = load_config(args.config.as_ref());

    let base_cache_dir = config
        .cache_dir
        .map(|p| {
            let p = if p.starts_with("~") {
                if let Some(home) = dirs::home_dir() {
                    PathBuf::from(p.replacen("~", home.to_str().unwrap_or(""), 1))
                } else {
                    PathBuf::from(p)
                }
            } else {
                PathBuf::from(p)
            };
            p
        })
        .or_else(|| dirs::cache_dir().map(|p| p.join("gitmon")))
        .expect("Could not determine cache directory");

    fs::create_dir_all(&base_cache_dir).expect("Failed to create cache directory");

    let state_file = base_cache_dir.join("state.json");
    let mut state = load_state(&state_file);

    let mut reports: Vec<RepoReport> = Vec::new();

    for repo_entry in &config.repos {
        let url = repo_entry.url();
        debug!("Checking remote repo: {}", url);
        match clone_or_update_repo(url, &base_cache_dir) {
            Ok(local_path) => {
                let branches = repo_entry
                    .branches()
                    .map(|b| b.to_vec())
                    .unwrap_or_else(|| {
                        get_all_remote_branches(&local_path).unwrap_or_default()
                    });

                for branch in &branches {
                    let state_key = format!("{}::{}", url, branch);
                    let last_seen_id = state.last_seen.get(&state_key).cloned();
                    match get_new_commits_since(
                        &local_path,
                        branch,
                        last_seen_id.as_deref(),
                        config.max_commits,
                    ) {
                        Ok(commits) if !commits.is_empty() => {
                            state.last_seen.insert(state_key, commits[0].id.clone());
                            reports.push(RepoReport {
                                url: url.to_string(),
                                branch: branch.clone(),
                                commits,
                            });
                        }
                        Ok(_) => info!("No new commits in {} [{}]", url, branch),
                        Err(e) => eprintln!("Failed to read commits from {} [{}]: {}", url, branch, e),
                    }
                }
            }
            Err(e) => eprintln!("Failed to prepare repo {}: {}", url, e),
        }
    }

    if !reports.is_empty() {
        let html = build_html_report_with_template(&reports, config.template_path.as_deref());

        if let Some(output_path) = args.output {
            match fs::write(&output_path, &html) {
                Ok(_) => info!("Report written to {:?}", output_path),
                Err(e) => eprintln!("Failed to write report: {}", e),
            }
        } else {
            match send_email(html, &config.from, &config.to, &config.token) {
                Ok(_) => info!("Email sent successfully."),
                Err(e) => eprintln!("Failed to send email: {}", e),
            }
        }

        save_state(&state, &state_file);
    } else {
        info!("No new commits found.");
    }
}
