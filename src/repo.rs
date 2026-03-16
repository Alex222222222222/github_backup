use std::collections::HashMap;

use futures_util::StreamExt;
use log::debug;

use crate::config::CONFIG;

pub struct Repo {
    pub name: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub archive_date: Option<chrono::DateTime<chrono::Utc>>,
}
impl Repo {
    fn url(&self) -> String {
        format!(
            "https://{}@github.com/{}/{}.git",
            CONFIG.github_token, CONFIG.github_username, self.name
        )
    }
}

pub async fn get_all_repos() -> anyhow::Result<Vec<Repo>> {
    debug!(
        "Starting to fetch all repos for user {}",
        CONFIG.github_username
    );
    #[derive(serde::Deserialize)]
    struct RepoRaw {
        pub name: String,
        pub updated_at: String,
    }

    /*
    * repos=$(curl -s \
      -H "Authorization: token $TOKEN" \
      "https://api.github.com/user/repos?affiliation=owner&per_page=$PER_PAGE&page=$page" 2>/dev/null)
    */
    let client = reqwest::Client::new();
    let mut page = 1;
    let mut repos = Vec::new();

    loop {
        let url = format!(
            "https://api.github.com/user/repos?affiliation=owner&per_page={}&page={}",
            CONFIG.per_page, page
        );
        let response = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "Alex222222222222")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("token {}", CONFIG.github_token),
            )
            .send()
            .await?;

        if !response.status().is_success() {
            debug!(
                "Failed to fetch repos from page {}: HTTP {}",
                page,
                response.status()
            );
            break;
        }

        let mut page_repos: Vec<RepoRaw> = response.json().await?;
        if page_repos.is_empty() {
            debug!("No more repos found on page {}, stopping fetch", page);
            break;
        }

        debug!("Fetched {} repos from page {}", page_repos.len(), page);

        repos.append(&mut page_repos);
        page += 1;
    }

    const MIN_UTC: chrono::DateTime<chrono::Utc> = chrono::DateTime::<chrono::Utc>::MIN_UTC;
    let mut all_archive_dates = get_all_repo_archive_dates().await?;

    debug!(
        "Fetched {} repos, {} archived repos",
        repos.len(),
        all_archive_dates.len()
    );

    Ok(repos
        .into_iter()
        .map(|repo| Repo {
            archive_date: all_archive_dates.remove(&repo.name),
            name: repo.name,
            updated_at: chrono::DateTime::parse_from_rfc3339(&repo.updated_at)
                .map_or(MIN_UTC, |dt| dt.with_timezone(&chrono::Utc)),
        })
        .collect())
}

async fn get_all_repo_archive_dates()
-> anyhow::Result<HashMap<String, chrono::DateTime<chrono::Utc>>> {
    // list all archived repos in s3_object_store/prefix, the name is repo_name.tar.zst
    let object_store = crate::s3::S3_OBJECT_STORE.lock().await;
    let mut archive_dates = HashMap::new();
    let prefix_path = object_store::path::Path::parse(&CONFIG.s3_path_prefix)?;
    let mut stream = object_store.list(Some(&prefix_path));
    while let Some(object) = stream.next().await {
        let object = match object {
            Ok(obj) => obj,
            Err(e) => {
                eprintln!("Failed to list object: {}", e);
                continue;
            }
        };
        let key = object.location.filename();
        if let Some(key) = key {
            if let Some(repo_name) = key.trim_matches('/').strip_suffix(".tar.zst") {
                archive_dates.insert(repo_name.to_string(), object.last_modified);
            }
        }
    }
    Ok(archive_dates)
}

pub async fn clone_repo(repo: &Repo) -> anyhow::Result<()> {
    // make sure work_dir/clone exists
    let clone_dir = std::path::Path::new(&CONFIG.work_dir).join("clone");
    tokio::fs::create_dir_all(&clone_dir).await?;

    // use tokio::Command to run git clone --mirror repo.url
    let output = tokio::process::Command::new("git")
        .arg("clone")
        .arg("--mirror")
        .arg(repo.url())
        .current_dir(clone_dir)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to clone repo {}: {}",
            repo.name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

pub async fn archive_repo(repo: &Repo) -> anyhow::Result<()> {
    // tar --use-compress-program="zstd --ultra -22" -cf "work_dir/archive/$name.tar.zst" -C "work_dir/clone" "name.git"
    // make sure work_dir/archive exists
    let archive_dir = std::path::Path::new(&CONFIG.work_dir).join("archive");
    tokio::fs::create_dir_all(&archive_dir).await?;
    let output = tokio::process::Command::new("tar")
        .arg("--use-compress-program=\"zstd --ultra -22\"")
        .arg("-cf")
        .arg(archive_dir.join(format!("{}.tar.zst", repo.name)))
        .arg("-C")
        .arg(std::path::Path::new(&CONFIG.work_dir).join("clone"))
        .arg(format!("{}.git", repo.name))
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to archive repo {}: {}",
            repo.name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}
