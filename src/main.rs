use github_backup::repo;
use log::{debug, error, info};

#[tokio::main]
async fn main() {
    env_logger::init();

    debug!("Starting GitHub backup process...");

    #[cfg(debug_assertions)]
    {
        extern crate dotenv;

        use dotenv::dotenv;
        dotenv().ok();
    }

    let object_store = github_backup::s3::create_remote_s3_object_store()
        .await
        .expect("Failed to create S3 object store");

    let repos = repo::get_all_repos(&object_store)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| {
            // never archived, or archived before the last update
            r.archive_date.map(|d| d < r.updated_at).unwrap_or(true)
        });
    for repo in repos {
        info!("Cloning repo: {}", repo.name);
        if let Err(e) = repo::clone_repo(&repo).await {
            error!("Failed to clone repo {}: {}", repo.name, e);
            continue;
        }
        info!("Archiving repo: {}", repo.name);
        if let Err(e) = repo::archive_repo(&repo).await {
            error!("Failed to archive repo {}: {}", repo.name, e);
            continue;
        }
        info!("Uploading archive of repo: {}", repo.name);
        if let Err(e) = repo::upload_archive(&object_store, &repo).await {
            error!("Failed to upload archive of repo {}: {}", repo.name, e);
            continue;
        }
    }
}
