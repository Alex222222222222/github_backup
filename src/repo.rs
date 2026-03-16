use std::{collections::HashMap, path::PathBuf};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use log::{debug, error};
use object_store::{ObjectStoreExt, UploadPart};
use tokio::io::AsyncReadExt;

use crate::config::CONFIG;

const CHUNK_SIZE: usize = 8 * 1024 * 1024;

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
                error!("Failed to list object: {}", e);
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
        return Ok(());
    }

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

pub async fn upload_archive(repo: &Repo) -> anyhow::Result<()> {
    let archive_path = std::path::Path::new(&CONFIG.work_dir)
        .join("archive")
        .join(format!("{}.tar.zst", repo.name));
    let archive_upload_path = object_store::path::Path::parse(&format!(
        "{}/{}.tar.zst",
        CONFIG.s3_path_prefix.trim_matches('/'),
        repo.name
    ))?;
    upload_muiltipart(&archive_path, &archive_upload_path).await
}

pub async fn upload_muiltipart(
    origin: &PathBuf,
    target: &object_store::path::Path,
) -> anyhow::Result<()> {
    let object_store = crate::s3::S3_OBJECT_STORE.lock().await;
    let mut put_multipart = object_store.put_multipart(target).await?;
    // open the origin, and read it in 8MB chunks, and put it to s3_object_store
    let mut file = tokio::fs::File::open(origin).await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let (error_tx, mut error_rx) = tokio::sync::mpsc::channel::<anyhow::Error>(2);
    let (tx_closed_indicater, rx_closed_indicater) = tokio::sync::oneshot::channel::<()>();

    // spawn a task to wait for the rx to be closed, and then send a signal to the tx_closed_indicater
    tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            let _ = f.await;
        }
        let _ = tx_closed_indicater.send(());
    });

    loop {
        // check if there is any error from the error_rx, if there is, return the error
        if let Ok(e) = error_rx.try_recv() {
            return Err(e);
        }

        let mut buffer = vec![0u8; CHUNK_SIZE];
        let n = file.read(&mut buffer).await?;
        buffer.truncate(n);
        debug!("Read {} bytes from file {}", n, origin.display());
        if n == 0 {
            break;
        }
        let b: Bytes = Bytes::from(buffer.into_boxed_slice());
        let upload_part = put_multipart.put_part(object_store::PutPayload::from(b));
        let error_tx_c = error_tx.clone();
        let f = tokio::spawn(async move {
            let r = upload_part.await;
            if let Err(e) = r {
                error!("Failed to upload part: {}", e);
                let _ = error_tx_c.send(anyhow::anyhow!(e)).await;
            }
        });
        tx.send(f).await?;
    }
    drop(tx); // closed the sender

    // wait for the closed indicater to receive something, which means the rx is closed
    let _ = rx_closed_indicater.await;
    put_multipart.complete().await?;

    Ok(())
}

pub async fn upload_muiltipart1(
    origin: &PathBuf,
    target: &object_store::path::Path,
) -> anyhow::Result<()> {
    let object_store = crate::s3::S3_OBJECT_STORE.lock().await;
    // open the origin, and read it in 8MB chunks, and put it to s3_object_store
    let mut file = tokio::fs::File::open(origin).await?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).await?;
    let b: Bytes = Bytes::from(buffer.into_boxed_slice());
    object_store
        .put(&target, object_store::PutPayload::from(b))
        .await
        .unwrap();

    Ok(())
}
