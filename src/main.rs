use github_backup::repo;
use log::{debug, error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    debug!("Starting GitHub backup process...");

    #[cfg(debug_assertions)]
    {
        extern crate dotenv;

        use dotenv::dotenv;
        dotenv().ok();
    }

    let object_store = github_backup::s3::create_remote_s3_object_store().await?;

    let repos = repo::get_all_repos(&object_store)
        .await?
        .into_iter()
        .filter(|r| {
            // never archived, or archived before the last update
            r.archive_date.map(|d| d < r.updated_at).unwrap_or(true)
        });
    let mut failed_repositories = Vec::new();
    for repo in repos {
        info!("Cloning repo: {}", repo.name);
        if let Err(e) = repo::clone_repo(&repo).await {
            error!("Failed to clone repo {}: {}", repo.name, e);
            failed_repositories.push(repo.name);
            continue;
        }
        info!("Archiving repo: {}", repo.name);
        if let Err(e) = repo::archive_repo(&repo).await {
            error!("Failed to archive repo {}: {}", repo.name, e);
            failed_repositories.push(repo.name);
            continue;
        }
        info!("Uploading archive of repo: {}", repo.name);
        if let Err(e) = repo::upload_archive(&object_store, &repo).await {
            error!("Failed to upload archive of repo {}: {}", repo.name, e);
            failed_repositories.push(repo.name);
            continue;
        }
    }

    finish_backup(&failed_repositories)
}

fn finish_backup(failed_repositories: &[String]) -> anyhow::Result<()> {
    if failed_repositories.is_empty() {
        return Ok(());
    }

    anyhow::bail!(
        "Backup failed for {} repositories: {}",
        failed_repositories.len(),
        failed_repositories.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_result_succeeds_when_no_repositories_failed() {
        assert!(finish_backup(&[]).is_ok());
    }

    #[test]
    fn backup_result_fails_when_a_repository_failed() {
        let error = finish_backup(&["example".to_string()]).unwrap_err();

        assert!(error.to_string().contains("example"));
    }
}
