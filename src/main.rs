use github_backup::repo;
use log::{debug, error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    debug!("Starting backup process...");

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
        .filter(should_process_repo);
    let mut failed_repositories = Vec::new();
    for repo in repos {
        info!("Synchronizing repo: {}", repo.name);
        let synchronized_state = match repo::clone_repo(&repo).await {
            Ok(state) => state,
            Err(e) => {
                error!("Failed to synchronize repo {}: {}", repo.name, e);
                failed_repositories.push(repo.name.clone());
                continue;
            }
        };
        info!("Archiving repo: {}", repo.name);
        if let Err(e) = repo::archive_repo(&repo).await {
            error!("Failed to archive repo {}: {}", repo.name, e);
            failed_repositories.push(repo.name.clone());
            continue;
        }
        info!("Uploading archive of repo: {}", repo.name);
        if let Err(e) = repo::upload_archive(&object_store, &repo).await {
            error!("Failed to upload archive of repo {}: {}", repo.name, e);
            failed_repositories.push(repo.name.clone());
            continue;
        }
        if let Some(state) = synchronized_state
            && let Err(e) = repo::upload_state(&object_store, &repo, &state).await
        {
            error!("Failed to upload state of repo {}: {}", repo.name, e);
            failed_repositories.push(repo.name.clone());
        }
    }

    finish_backup(&failed_repositories)
}

fn should_process_repo(repo: &repo::Repo) -> bool {
    repo.needs_backup()
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
    use github_backup::repo::{Repo, RepoSource};
    use std::path::PathBuf;

    fn test_ssh_repo_with_states(
        name: &str,
        remote_state: Option<&str>,
        archived_state: Option<&str>,
    ) -> Repo {
        Repo {
            name: name.into(),
            updated_at: None,
            archive_date: Some(100),
            source: RepoSource::Ssh {
                url: format!("backup@git.example.test:/srv/git/{name}"),
                private_key_path: PathBuf::from("/run/secrets/id_ed25519"),
                s3_path_prefix: "ssh/".into(),
            },
            remote_state: remote_state.map(str::to_owned),
            archived_state: archived_state.map(str::to_owned),
        }
    }

    #[test]
    fn backup_result_succeeds_when_no_repositories_failed() {
        assert!(finish_backup(&[]).is_ok());
    }

    #[test]
    fn backup_result_fails_when_a_repository_failed() {
        let error = finish_backup(&["example".to_string()]).unwrap_err();

        assert!(error.to_string().contains("example"));
    }

    #[test]
    fn backup_selection_uses_source_aware_repo_state() {
        let repo = test_ssh_repo_with_states("project", Some("same"), Some("same"));

        assert!(!should_process_repo(&repo));
    }
}
