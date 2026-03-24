use std::{collections::HashMap, path::PathBuf};

use futures_util::{AsyncWriteExt, StreamExt};
use log::{debug, error};
use opendal::Operator;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::config::CONFIG;

const CHUNK_SIZE: usize = 8 * 1024 * 1024;

pub struct Repo {
    pub name: String,
    pub updated_at: i64,
    pub archive_date: Option<i64>,
}
impl Repo {
    fn url(&self) -> String {
        format!(
            "https://{}@github.com/{}/{}.git",
            CONFIG.github_token, CONFIG.github_username, self.name
        )
    }
}

pub async fn get_all_repos(object_store: &Operator) -> anyhow::Result<Vec<Repo>> {
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
    let mut all_archive_dates = get_all_repo_archive_dates(object_store).await?;

    debug!(
        "Fetched {} repos, {} archived repos",
        repos.len(),
        all_archive_dates.len()
    );

    Ok(repos
        .into_iter()
        .map(|repo| Repo {
            archive_date: all_archive_dates.remove(&repo.name).flatten(),
            name: repo.name,
            updated_at: chrono::DateTime::parse_from_rfc3339(&repo.updated_at)
                .map_or(MIN_UTC.timestamp(), |dt| {
                    dt.with_timezone(&chrono::Utc).timestamp()
                }),
        })
        .collect())
}

async fn get_all_repo_archive_dates(
    object_store: &Operator,
) -> anyhow::Result<HashMap<String, Option<i64>>> {
    // list all archived repos in s3_object_store/prefix, the name is repo_name.tar.zst
    let mut archive_dates = HashMap::new();
    let mut lister = object_store.lister(&CONFIG.s3_path_prefix).await?;
    while let Some(object) = lister.next().await {
        let object = match object {
            Ok(obj) => obj,
            Err(e) => {
                error!("Failed to list object: {}", e);
                continue;
            }
        };
        let key = object.name();
        if let Some(repo_name) = key.trim_matches('/').strip_suffix(".tar.zst") {
            archive_dates.insert(
                repo_name.to_string(),
                object
                    .metadata()
                    .last_modified()
                    .map(|t| t.into_inner().as_second()),
            );
        }
    }
    Ok(archive_dates)
}

pub async fn clone_repo(repo: &Repo) -> anyhow::Result<()> {
    // make sure work_dir/clone exists
    let clone_dir = std::path::Path::new(&CONFIG.work_dir).join("clone");
    tokio::fs::create_dir_all(&clone_dir).await?;

    // test if work_dir/clone/name.git already exists,
    // if exists,`git -C "work_dir/clone/name.git" remote update`
    let repo_dir = clone_dir.join(format!("{}.git", repo.name));
    if repo_dir.exists() {
        debug!("Repo {} already exists, updating remote", repo.name);
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .arg("remote")
            .arg("update")
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "Failed to update remote for repo {}: {}",
                repo.name,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    } else {
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
    }

    // gc the repo to reduce the size
    let output = tokio::process::Command::new("git")
        .arg("gc")
        .arg("--aggressive")
        .arg("--prune=now")
        .current_dir(&repo_dir)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to gc repo {}: {}",
            repo.name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // repack the repo to reduce the size
    let output = tokio::process::Command::new("git")
        .arg("repack")
        .arg("-a")
        .arg("-d")
        .arg("--window=250")
        .arg("--depth=250")
        .current_dir(&repo_dir)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to repack repo {}: {}",
            repo.name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

pub async fn archive_repo(repo: &Repo) -> anyhow::Result<()> {
    // tar --zstd -cf "work_dir/archive/$name.tar.zst" -C "work_dir/clone" "name.git"
    // make sure work_dir/archive exists
    let archive_dir = std::path::Path::new(&CONFIG.work_dir).join("archive");
    tokio::fs::create_dir_all(&archive_dir).await?;
    let output = tokio::process::Command::new("tar")
        .arg("--zstd")
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

pub async fn upload_archive(object_store: &Operator, repo: &Repo) -> anyhow::Result<()> {
    let archive_path = std::path::Path::new(&CONFIG.work_dir)
        .join("archive")
        .join(format!("{}.tar.zst", repo.name));
    let archive_upload_path = &format!(
        "{}/{}.tar.zst",
        CONFIG.s3_path_prefix.trim_matches('/'),
        repo.name
    );
    upload_muiltipart(object_store, &archive_path, archive_upload_path).await
}

pub async fn upload_muiltipart(
    object_store: &Operator,
    origin: &PathBuf,
    target: &str,
) -> anyhow::Result<()> {
    let mut writer = object_store
        .writer_with(target)
        .chunk(CHUNK_SIZE)
        .concurrent(4)
        .await?
        .into_futures_async_write();
    let mut file = tokio::fs::File::open(origin).await?.compat();
    futures_util::io::copy(&mut file, &mut writer).await?;
    writer.close().await?;

    Ok(())
}
