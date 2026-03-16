use std::collections::HashMap;

use futures_util::StreamExt;

use crate::config::CONFIG;

pub struct Repo {
    pub name: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub archive_date: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn get_all_repos() -> anyhow::Result<Vec<Repo>> {
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
            .header("Authorization", format!("token {}", CONFIG.github_token))
            .send()
            .await?;

        if !response.status().is_success() {
            break;
        }

        let mut page_repos: Vec<RepoRaw> = response.json().await?;
        if page_repos.is_empty() {
            break;
        }

        repos.append(&mut page_repos);
        page += 1;
    }

    const MIN_UTC: chrono::DateTime<chrono::Utc> = chrono::DateTime::<chrono::Utc>::MIN_UTC;
    let mut all_archive_dates = get_all_repo_archive_dates().await?;

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
