use crate::config::CONFIG;

pub struct Repo {
    pub name: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
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

    Ok(repos
        .into_iter()
        .map(|repo| Repo {
            name: repo.name,
            updated_at: chrono::DateTime::parse_from_rfc3339(&repo.updated_at)
                .map_or(MIN_UTC, |dt| dt.with_timezone(&chrono::Utc)),
        })
        .collect())
}
